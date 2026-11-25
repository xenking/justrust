//! Size cap and LRU eviction for the shared dependency cache, and
//! `justrust cache`.
//!
//! Recency: a hit touches `o/<key>/meta.json` (see `depcache::touch`), a store
//! creates it fresh, so its mtime is the entry's last use.
//!
//! Eviction runs in a detached background process after a recorded run, at
//! most once per [`GC_INTERVAL_SECS`], so builds never wait for it. Only one
//! collector runs at a time (`gc.lock` flock). When the cache exceeds the cap
//! (default a quarter of the free disk, 30 to 150 GB, `JUSTRUST_DEPCACHE_MAX`, e.g. `50G`, `800M`, `0` = no cap),
//! the least recently used entries are removed until it is under 90% of it.
//!
//! Safety with concurrent builds:
//! - An entry is first renamed into `trash/` (atomic), then deleted. A restore
//!   that already opened a file keeps reading it, one that has not yet fails to
//!   open it and compiles normally (restore fails open and rustc overwrites
//!   any partially restored outputs).
//! - Entries used within [`GRACE_SECS`] are never evicted, and an entry whose
//!   `meta.json` was touched between the scan and the rename is put back.
//! - Manifest lines pointing at evicted entries simply miss.

use crate::depcache;
use crate::paths;
use crate::sys::Detach;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Cap floor on roomy disks, and the cap when filesystem size cannot be read.
pub const DEFAULT_MAX_BYTES: u64 = 30 * 1024 * 1024 * 1024;
/// Largest default cap on a roomy disk.
const ROOMY_MAX_BYTES: u64 = 150 * 1024 * 1024 * 1024;
/// Cap floor on compact disks (e.g. 48-192 GiB hosted guest volumes).
const SMALL_DISK_FLOOR_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Disks smaller than this scale the depcache cap down proportionally so
/// depcache alone does not consume 40-60% of a hosted account disk.
const SMALL_DISK_THRESHOLD_BYTES: u64 = 200 * 1024 * 1024 * 1024;
const GC_INTERVAL_SECS: f64 = 600.0;
/// Entries used this recently are never evicted (`JUSTRUST_DEPCACHE_GRACE`
/// overrides it, for stress tests).
const GRACE_SECS: f64 = 600.0;
const LOW_WATER: f64 = 0.9;

/// Parse `30G`, `512M`, `1.5T`, `123456` (bytes). `0` means no cap.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = match s.char_indices().find(|(_, c)| c.is_ascii_alphabetic()) {
        Some((i, _)) => {
            let unit = s[i..].to_ascii_uppercase();
            let unit = unit.trim_end_matches('B').trim_end_matches('I');
            let m: u64 = match unit {
                "" => 1,
                "K" => 1 << 10,
                "M" => 1 << 20,
                "G" => 1 << 30,
                "T" => 1 << 40,
                _ => return None,
            };
            (&s[..i], m)
        }
        None => (s, 1),
    };
    let v: f64 = num.trim().parse().ok()?;
    (v >= 0.0).then_some((v * mult as f64) as u64)
}

pub fn max_bytes() -> u64 {
    std::env::var("JUSTRUST_DEPCACHE_MAX")
        .ok()
        .and_then(|v| parse_size(&v))
        .unwrap_or_else(|| {
            let space = depcache::cache_dir().and_then(|d| disk_space(&d));
            default_cap_for_disk(space)
        })
}

/// Default cap: on roomy disks (>= 200 GiB total), a quarter of free space
/// clamped to 30-150 GiB (FINDINGS 49). On compact disks (< 200 GiB, e.g.
/// hosted guest volumes), scale proportionally to 12.5-20% of total disk
/// (at most 8 GiB floor, 30 GiB ceiling) so depcache does not trigger
/// unnecessary host disk expansion.
pub(crate) fn default_cap_for_disk(space: Option<(u64, u64)>) -> u64 {
    match space {
        Some((free, total)) if total > 0 && total < SMALL_DISK_THRESHOLD_BYTES => {
            let ceiling = (total / 5).min(DEFAULT_MAX_BYTES);
            let floor = (total / 8).min(SMALL_DISK_FLOOR_BYTES).min(ceiling);
            (free / 4).clamp(floor, ceiling)
        }
        Some((free, _)) => (free / 4).clamp(DEFAULT_MAX_BYTES, ROOMY_MAX_BYTES),
        None => DEFAULT_MAX_BYTES,
    }
}

/// `(free, total)` bytes on the filesystem holding `dir` (or its nearest
/// existing ancestor).
fn disk_space(dir: &Path) -> Option<(u64, u64)> {
    crate::sys::disk_space(dir.ancestors().find(|p| p.exists())?)
}

#[derive(Debug, Clone)]
struct Entry {
    path: PathBuf,
    bytes: u64,
    last_used: f64,
    /// A linked executable or test binary: hundreds of MB, rarely hit, and
    /// held as a clone of a file the next build overwrites. Collected on its
    /// own, shorter clock (see `store::ephemeral_idle_secs`).
    ephemeral: bool,
}

/// Smallest executable that counts as a link output worth its own policy.
const EPHEMERAL_MIN_BYTES: u64 = 8 * 1024 * 1024;

/// Whether an entry holds a large executable (test binary, bin): a file with
/// no extension, or `.exe`, at the top of the entry.
fn is_ephemeral(dir: &Path) -> bool {
    std::fs::read_dir(dir).into_iter().flatten().flatten().any(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        let exe = !name.starts_with('.')
            && (name.ends_with(".exe") || (!name.contains('.') && name != "out"));
        exe && e.metadata().is_ok_and(|m| m.is_file() && m.len() >= EPHEMERAL_MIN_BYTES)
    })
}

#[derive(Debug, Default)]
pub struct Scan {
    entries: Vec<Entry>,
    pub bytes: u64,
    pub manifests: usize,
    pub manifest_bytes: u64,
}

fn mtime(m: &std::fs::Metadata) -> f64 {
    crate::sys::mtime_f64(m)
}

/// Disk blocks actually allocated (reflinked extents still count in full),
/// recursively (build-script entries keep `OUT_DIR` trees under `out/`).
fn dir_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| Some((e.path(), e.metadata().ok()?)))
        .map(|(p, m)| match m.is_dir() {
            true => dir_bytes(&p),
            false => crate::sys::disk_bytes(&m),
        })
        .sum()
}

/// Inventory committed entries only. Status must not inspect or clean partial stores.
pub fn scan(root: &Path) -> Scan {
    inventory(root, |_| {})
}

fn inventory(root: &Path, mut on_partial: impl FnMut(&std::fs::DirEntry)) -> Scan {
    let mut s = Scan::default();
    for e in std::fs::read_dir(root.join("o"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            on_partial(&e);
            continue;
        }
        let path = e.path();
        let last_used = std::fs::metadata(path.join("meta.json"))
            .or_else(|_| e.metadata())
            .map(|m| mtime(&m))
            .unwrap_or(0.0);
        let bytes = dir_bytes(&path);
        s.bytes += bytes;
        let ephemeral = is_ephemeral(&path);
        s.entries.push(Entry {
            path,
            bytes,
            last_used,
            ephemeral,
        });
    }
    for e in std::fs::read_dir(root.join("m"))
        .into_iter()
        .flatten()
        .flatten()
    {
        if let Ok(m) = e.metadata() {
            s.manifests += 1;
            s.manifest_bytes += crate::sys::disk_bytes(&m);
        }
    }
    s.bytes += s.manifest_bytes;
    s
}

#[derive(Debug, Default, PartialEq)]
pub struct Pruned {
    pub entries: usize,
    pub bytes: u64,
}

/// Link outputs (executables, test binaries) are held by the store only as a
/// convenience for a revert, so they are not given the durable grace.
const EPHEMERAL_GRACE_SECS: f64 = 120.0;

/// What a collection may take.
#[derive(Debug, Clone)]
pub struct Rules {
    /// Size cap over all roots together (0 = none). Eviction stops at
    /// `LOW_WATER` of it.
    pub cap: u64,
    /// Evict everything outside grace and pins.
    pub all: bool,
    /// Entries used this recently stay.
    pub grace: f64,
    /// Entries used at or after this time may be in use by a running build
    /// (see `store::pin_floor`).
    pub pin_floor: Option<f64>,
    /// Idle link outputs older than this go regardless of the cap.
    pub ephemeral_idle: f64,
    /// Allocated bytes of link outputs the store keeps.
    pub ephemeral_cap: u64,
}

impl Rules {
    /// Routine collection under a cap, with the running builds' pins.
    pub fn routine(cap: u64, all: bool) -> Rules {
        Rules {
            cap,
            all,
            grace: std::env::var("JUSTRUST_DEPCACHE_GRACE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(GRACE_SECS),
            pin_floor: crate::store::pin_floor(),
            ephemeral_idle: crate::store::ephemeral_idle_secs(),
            ephemeral_cap: crate::store::ephemeral_cap(cap),
        }
    }
}

/// Evict least recently used entries until the cache is under `LOW_WATER`
/// of `max` (or everything not in use, when `max` is 0 and `all` is set).
/// Link outputs additionally expire on their own idle clock and cap.
pub fn prune(root: &Path, max: u64, all: bool) -> Pruned {
    prune_roots(&[root], &Rules::routine(max, all))
}

/// Move one entry into `trash/` and report whether it left. A restore that
/// already opened a file keeps reading it, one that has not fails open.
fn evict(root: &Path, e: &Entry) -> bool {
    let Some(name) = e.path.file_name() else {
        return false;
    };
    let trash = root.join("trash");
    let _ = std::fs::create_dir_all(&trash);
    let dst = trash.join(format!("{}.{}", name.to_string_lossy(), std::process::id()));
    if std::fs::rename(&e.path, &dst).is_err() {
        return false;
    }
    // Hit between scan and rename: put it back.
    let touched = std::fs::metadata(dst.join("meta.json"))
        .map(|m| mtime(&m) > e.last_used + 0.5)
        .unwrap_or(false);
    !(touched && std::fs::rename(&dst, &e.path).is_ok())
}

/// One collection over every root, LRU across all of them.
///
/// Order: (1) idle link outputs, (2) link outputs over their own cap, (3)
/// anything, least recently used first, until the roots together are under
/// `LOW_WATER` of the cap. Entries inside the grace period or pinned by a
/// running build's lease are skipped, not a reason to stop: a later entry
/// can still be old enough.
pub fn prune_roots(roots: &[&Path], rules: &Rules) -> Pruned {
    let mut scans: Vec<Scan> = roots
        .iter()
        .map(|root| {
            inventory(root, |e| {
                // Half-written store from a crashed process: collect once stale.
                if let Ok(m) = e.metadata()
                    && paths::now() - mtime(&m) > 3600.0
                {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            })
        })
        .collect();
    let now = paths::now();
    let mut out = Pruned::default();
    // (root index, entry), oldest use first.
    let mut all: Vec<(usize, Entry)> = scans
        .iter_mut()
        .enumerate()
        .flat_map(|(i, s)| std::mem::take(&mut s.entries).into_iter().map(move |e| (i, e)))
        .collect();
    all.sort_by(|a, b| a.1.last_used.total_cmp(&b.1.last_used));
    let mut total: u64 = scans.iter().map(|s| s.bytes).sum();
    let mut eph: u64 = all.iter().filter(|(_, e)| e.ephemeral).map(|(_, e)| e.bytes).sum();
    let mut gone = vec![false; all.len()];

    let protected = |e: &Entry| {
        let grace = if e.ephemeral { EPHEMERAL_GRACE_SECS } else { rules.grace };
        now - e.last_used < grace || rules.pin_floor.is_some_and(|f| e.last_used >= f)
    };
    let mut take = |i: usize, total: &mut u64, eph: &mut u64, gone: &mut Vec<bool>| {
        let (r, e) = &all[i];
        if evict(roots[*r], e) {
            gone[i] = true;
            *total = total.saturating_sub(e.bytes);
            if e.ephemeral {
                *eph = eph.saturating_sub(e.bytes);
            }
            out.entries += 1;
            out.bytes += e.bytes;
        }
    };

    // 1. Link outputs nobody has needed for a while.
    for i in 0..all.len() {
        let e = &all[i].1;
        if e.ephemeral && now - e.last_used > rules.ephemeral_idle && !protected(e) {
            take(i, &mut total, &mut eph, &mut gone);
        }
    }
    // 2. Link outputs over their own cap.
    for i in 0..all.len() {
        if eph <= rules.ephemeral_cap {
            break;
        }
        let e = &all[i].1;
        if !gone[i] && e.ephemeral && !protected(e) {
            take(i, &mut total, &mut eph, &mut gone);
        }
    }
    // 3. The cap.
    let target = if rules.all {
        0
    } else if rules.cap == 0 {
        u64::MAX
    } else {
        (rules.cap as f64 * LOW_WATER) as u64
    };
    if rules.all || (rules.cap != 0 && total > rules.cap) {
        for i in 0..all.len() {
            if total <= target {
                break;
            }
            if !gone[i] && !protected(&all[i].1) {
                take(i, &mut total, &mut eph, &mut gone);
            }
        }
    }
    for root in roots {
        clear_trash(root);
        drop_dead_manifest_lines(root);
    }
    out
}

/// Smallest private size worth evicting in a low-disk reclaim: an entry that
/// is a clone of a live target file frees nothing until that file goes.
const RECLAIM_MIN_PRIVATE: u64 = 1024 * 1024;

fn reclaim_in(roots: &[PathBuf], want: u64, ephemeral_only: bool, idle_scale: f64) -> u64 {
    let now = paths::now();
    let mut all: Vec<(usize, Entry)> = Vec::new();
    for (i, root) in roots.iter().enumerate() {
        all.extend(inventory(root, |_| {}).entries.into_iter().map(|e| (i, e)));
    }
    all.sort_by(|a, b| a.1.last_used.total_cmp(&b.1.last_used));
    let mut freed = 0u64;
    for (r, e) in &all {
        if freed >= want {
            break;
        }
        if (ephemeral_only && !e.ephemeral) || now - e.last_used < EPHEMERAL_GRACE_SECS * idle_scale {
            continue;
        }
        // What deleting it frees now, not what it would weigh alone.
        let private = crate::store::dir_private(&e.path);
        if private < RECLAIM_MIN_PRIVATE {
            continue;
        }
        if evict(&roots[*r], e) {
            freed += private;
        }
    }
    for root in roots {
        clear_trash(root);
        drop_dead_manifest_lines(root);
    }
    freed
}

/// Low-disk reclaim, cheapest first: idle link outputs.
pub fn reclaim_ephemeral(roots: &[PathBuf], want: u64, idle_scale: f64) -> u64 {
    reclaim_in(roots, want, true, idle_scale)
}

/// Low-disk reclaim: least recently used entries that free real space.
pub fn reclaim_lru(roots: &[PathBuf], want: u64, idle_scale: f64) -> u64 {
    reclaim_in(roots, want, false, idle_scale)
}

/// Cap for one root: the local-unit cache has its own, the rest follow the
/// dependency cache's.
pub fn root_cap(root: &Path) -> u64 {
    if crate::localcache::cache_dir().is_some_and(|l| l == root) {
        crate::localcache::max_bytes()
    } else {
        max_bytes()
    }
}

/// Totals for `justrust store`: what the entries weigh as allocated blocks
/// and what deleting them would free now.
#[derive(Debug, Default)]
pub struct PhysScan {
    pub alloc: u64,
    pub private: u64,
    pub entries: usize,
    pub ephemeral: usize,
    pub ephemeral_alloc: u64,
}

pub fn scan_phys(root: &Path) -> PhysScan {
    let s = scan(root);
    let mut p = PhysScan {
        alloc: s.bytes,
        entries: s.entries.len(),
        ..Default::default()
    };
    for e in &s.entries {
        p.private += crate::store::dir_private(&e.path);
        if e.ephemeral {
            p.ephemeral += 1;
            p.ephemeral_alloc += e.bytes;
        }
    }
    p
}

fn clear_trash(root: &Path) {
    for e in std::fs::read_dir(root.join("trash"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let _ = std::fs::remove_dir_all(e.path());
    }
}

/// Rewrite manifests without lines whose outputs were evicted. A line
/// appended concurrently between read and rename can be lost, which only
/// costs a future miss.
fn drop_dead_manifest_lines(root: &Path) {
    let o = root.join("o");
    for e in std::fs::read_dir(root.join("m"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let path = e.path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let keep: Vec<&str> = text
            .lines()
            .filter(|l| {
                depcache::entry_out(l).is_some_and(|out| o.join(out).join("meta.json").exists())
            })
            .collect();
        if keep.len() == text.lines().count() {
            continue;
        }
        if keep.is_empty() {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        let mut body = keep.join("\n");
        body.push('\n');
        if std::fs::write(&tmp, body).is_ok() && std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

fn try_lock(path: &Path) -> Option<std::fs::File> {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()?;
    f.try_lock().is_ok().then_some(f)
}

/// Called after every recorded run. Costs one `stat` unless a collection is
/// due, then spawns a detached `justrust cache --auto` and returns.
pub fn maybe_spawn() {
    if !depcache::enabled() {
        return;
    }
    let Some(root) = depcache::cache_dir() else {
        return;
    };
    let stamp = root.join("gc.stamp");
    match std::fs::metadata(&stamp) {
        Ok(m) if paths::now() - mtime(&m) < GC_INTERVAL_SECS => return,
        Err(_) if !root.join("o").is_dir() => return,
        _ => {}
    }
    if std::fs::write(&stamp, b"").is_err() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = std::process::Command::new(exe)
        .args(["cache", "--auto"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .detached()
        .spawn();
}

/// Background collection: nice, lock, prune to the cap, log what happened.
fn auto(root: &Path) {
    crate::sys::lower_priority();
    let Some(_lock) = try_lock(&root.join("gc.lock")) else {
        return;
    };
    let start = paths::now();
    let mut p = prune(root, max_bytes(), false);
    // The store as a whole: depcache and local-unit cache under one cap, LRU
    // across both, so neither can starve the other of the disk.
    if paths::cache_namespace() == Ok(None)
        && let Some(local) = crate::localcache::cache_dir().filter(|l| l.join("o").is_dir())
    {
        let cap = crate::store::store_cap(disk_space(root));
        let both = prune_roots(&[root, &local], &Rules::routine(cap, false));
        p.entries += both.entries;
        p.bytes += both.bytes;
    }
    // Low disk: the volume, not the cap, is what a build dies on.
    if crate::store::guard_enabled()
        && let Some((free, total)) = disk_space(root)
    {
        let floor = crate::store::min_free(total);
        if free < floor {
            let freed = crate::store::reclaim(floor * 2 - free);
            p.bytes += freed;
        }
    }
    if crate::store::retention::due() {
        crate::store::retention::run();
    }
    if paths::cache_namespace() == Ok(None)
        && let Some(c) = paths::cache_root()
        && root == c.join("depcache")
    {
        prune_namespaces(&c.join("ns"), NAMESPACE_MAX_AGE_SECS);
    }
    if p.entries > 0 {
        let line = serde_json::json!({
            "t": start,
            "evicted": p.entries,
            "bytes": p.bytes,
            "secs": paths::now() - start,
        });
        let _ = paths::append_line(&root.join("gc.jsonl"), &line.to_string());
    }
}

/// Namespaced caches (`JUSTRUST_DEPCACHE_NAMESPACE`, one per cold benchmark
/// sample) unused for this long are removed whole by the default cache's
/// collection. Nothing else would ever reclaim them on a build machine.
const NAMESPACE_MAX_AGE_SECS: f64 = 2.0 * 86400.0;

/// Remove namespace dirs under `ns` whose newest entry (the dir, its cache
/// dirs and their `o/` dirs, which every store and restore touches) is older
/// than `max_age`.
fn prune_namespaces(ns: &Path, max_age: f64) -> usize {
    let Ok(rd) = std::fs::read_dir(ns) else {
        return 0;
    };
    let cutoff = paths::now() - max_age;
    let mut removed = 0;
    for d in rd.flatten() {
        let dir = d.path();
        if !d.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let newest = [
            dir.clone(),
            dir.join("depcache"),
            dir.join("depcache/o"),
            dir.join("depcache/m"),
            dir.join("localcache"),
            dir.join("localcache/o"),
        ]
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| mtime(&m))
        .fold(f64::MIN, f64::max);
        if newest < cutoff && std::fs::remove_dir_all(&dir).is_ok() {
            removed += 1;
        }
    }
    removed
}

fn gb(b: u64) -> String {
    format!("{:.2} GB", b as f64 / (1u64 << 30) as f64)
}

/// Hit/miss totals over recorded runs in the last `days`.
fn hit_rate(days: f64) -> (usize, usize, f64, usize) {
    let cutoff = paths::now() - days * 86400.0;
    let Ok(runs) = paths::runs_dir() else {
        return (0, 0, 0.0, 0);
    };
    let (mut hits, mut misses, mut saved, mut n) = (0, 0, 0.0, 0);
    for e in crate::runs::load_index() {
        if e.start < cutoff {
            continue;
        }
        let Ok(b) = std::fs::read(runs.join(&e.id).join("summary.json")) else {
            continue;
        };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&b) else {
            continue;
        };
        let u = &v["units"];
        let h = u["depcache_hits"].as_u64().unwrap_or(0) as usize;
        let m = u["depcache_misses"].as_u64().unwrap_or(0) as usize;
        if h + m > 0 {
            n += 1;
        }
        hits += h;
        misses += m;
        saved += u["depcache_saved_secs"].as_f64().unwrap_or(0.0);
    }
    (hits, misses, saved, n)
}

/// `justrust cache --misses [DAYS]`: every missed dependency unit of the
/// recorded runs, by reason (`depcache_why.rs`), then by package.
pub fn misses(days: f64) {
    let cutoff = paths::now() - days * 86400.0;
    let Ok(runs) = paths::runs_dir() else {
        return;
    };
    // category -> (units, secs), (package, reason) -> (units, secs)
    let mut by_why: std::collections::BTreeMap<String, (usize, f64)> = Default::default();
    let mut by_pkg: std::collections::HashMap<(String, String), (usize, f64)> = Default::default();
    // (workspace, command, toolchain) -> (runs, units, secs)
    let mut by_cmd: std::collections::HashMap<(String, String, String), (usize, usize, f64)> =
        Default::default();
    let (mut hits, mut total, mut secs) = (0usize, 0usize, 0.0);
    for e in crate::runs::load_index() {
        if e.start < cutoff {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(runs.join(&e.id).join("units.jsonl")) else {
            continue;
        };
        let (mut run_misses, mut run_secs) = (0usize, 0.0);
        for u in text
            .lines()
            .filter(|l| l.contains("\"depcache\""))
            .filter_map(|l| serde_json::from_str::<crate::shim::Unit>(l).ok())
        {
            match u.depcache.as_str() {
                "hit" | "prebuilt" | "remote" | "peer" => hits += 1,
                "miss" => {
                    total += 1;
                    secs += u.wall;
                    run_misses += 1;
                    run_secs += u.wall;
                    let c = crate::depcache_why::category(&u.depcache_why).to_owned();
                    let w = by_why.entry(c).or_default();
                    w.0 += 1;
                    w.1 += u.wall;
                    let reason = match u.depcache_why.is_empty() {
                        true => "unrecorded".to_owned(),
                        false => u.depcache_why,
                    };
                    let p = by_pkg
                        .entry((format!("{} {}", u.package, u.version), reason))
                        .or_default();
                    p.0 += 1;
                    p.1 += u.wall;
                }
                _ => {}
            }
        }
        if run_misses > 0
            && let Some(key) = run_command(&runs.join(&e.id))
        {
            let c = by_cmd.entry(key).or_default();
            c.0 += 1;
            c.1 += run_misses;
            c.2 += run_secs;
        }
    }
    if total == 0 {
        println!("no dependency cache misses in the last {days} days");
        return;
    }
    println!(
        "{total} misses, {hits} hits ({:.1}% hit rate) in the last {days} days, {:.0}s of rustc on misses",
        100.0 * hits as f64 / (hits + total) as f64,
        secs
    );
    let mut why: Vec<_> = by_why.into_iter().collect();
    why.sort_by(|a, b| b.1.1.total_cmp(&a.1.1));
    println!("\nreason                 units   share   rustc");
    for (c, (n, s)) in &why {
        println!(
            "{c:<22} {n:>5}  {:>5.1}%  {s:>6.0}s",
            100.0 * *n as f64 / total as f64
        );
    }
    if why.iter().any(|(c, _)| c == "unrecorded") {
        println!(
            "\nunrecorded: built by a justrust without miss reasons, or on a build host \
             whose client does not report them yet"
        );
    }
    let mut pkg: Vec<_> = by_pkg.into_iter().collect();
    pkg.sort_by(|a, b| b.1.1.total_cmp(&a.1.1));
    println!("\ncostliest misses (package, units, rustc, reason)");
    for ((p, r), (n, s)) in pkg.iter().take(25) {
        println!("{p:<34} {n:>4} {s:>7.1}s  {r}");
    }
    // What the prebuild fleet would have to build to turn these into store
    // hits: the store only matches the exact lockfile, features, profile and
    // toolchain, so candidates are whole commands, not crate names.
    let mut cmd: Vec<_> = by_cmd.into_iter().collect();
    cmd.sort_by(|a, b| b.1.2.total_cmp(&a.1.2));
    println!("\ncommands that missed most (runs, units, rustc, toolchain, workspace: command)");
    for ((ws, args, tc), (n, units, s)) in cmd.iter().take(20) {
        println!("{n:>4} {units:>6} {s:>8.0}s  {tc:<20} {ws}: {args}");
    }
    println!(
        "\nA public workspace becomes store hits as a `<git url> <dir> <command>` line in \
         justrust-cloud prebuild/workspaces.txt, built with a toolchain in the fleet matrix"
    );
}

/// (workspace, command, toolchain) of a recorded run, from its summary.
/// Test filters and anything after `--` are dropped: they do not move keys.
fn run_command(dir: &Path) -> Option<(String, String, String)> {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("summary.json")).ok()?).ok()?;
    let args: Vec<&str> = v
        .get("args")?
        .as_array()?
        .iter()
        .filter_map(|a| a.as_str())
        .take_while(|a| *a != "--")
        .collect();
    let ws = v
        .pointer("/git/root")
        .and_then(|r| r.as_str())
        .or_else(|| v.get("cwd").and_then(|c| c.as_str()))?;
    let tc = v
        .get("toolchain")
        .and_then(|t| t.as_str())
        .unwrap_or("system");
    Some((ws.to_owned(), args.join(" "), tc.to_owned()))
}

/// `justrust cache [--prune] [--clear]`.
pub fn command(
    prune_now: bool,
    clear: bool,
    auto_mode: bool,
    sync: bool,
    publish: Option<PathBuf>,
) -> Result<()> {
    let root = depcache::cache_dir().context("no cache directory")?;
    if let Some(dst) = publish {
        let p = crate::depcache_prebuilt::publish(&root, &dst)?;
        println!(
            "published {} new bundles ({}), {} manifest lines, {} build-script runs, {} skipped, to {}",
            p.bundles,
            gb(p.bytes),
            p.lines,
            p.scripts,
            p.skipped,
            dst.display()
        );
        return Ok(());
    }
    if auto_mode {
        auto(&root);
        return Ok(());
    }
    if sync {
        match crate::depcache_remote::sync(&root) {
            Some(s) => println!(
                "uploaded {} entries, {} skipped, {} failed",
                s.uploaded, s.skipped, s.failed
            ),
            None => {
                println!("nothing uploaded (empty queue, signed out, or another upload running)")
            }
        }
        return Ok(());
    }
    let max = max_bytes();
    if prune_now || clear {
        let _lock = try_lock(&root.join("gc.lock"))
            .context("another eviction is running, try again shortly")?;
        let p = prune(&root, max, clear);
        println!("evicted {} entries, {}", p.entries, gb(p.bytes));
    }
    let s = scan(&root);
    let cap = if max == 0 { "no cap".into() } else { gb(max) };
    println!("depcache  {}", root.display());
    println!(
        "size      {} of {} cap ({} entries, {} manifests)",
        gb(s.bytes),
        cap,
        s.entries.len(),
        s.manifests
    );
    if !depcache::enabled() {
        println!("status    disabled by JUSTRUST_DEPCACHE");
    }
    println!("{}", crate::depcache_remote::status_line(&root));
    println!("{}", crate::depcache_prebuilt::status_line(&root));
    if let (Some(old), Some(new)) = (
        s.entries.iter().map(|e| e.last_used).reduce(f64::min),
        s.entries.iter().map(|e| e.last_used).reduce(f64::max),
    ) {
        let now = paths::now();
        println!(
            "used      oldest entry last used {:.1}h ago, newest {:.0}s ago",
            (now - old) / 3600.0,
            now - new
        );
    }
    let (h, m, saved, n) = hit_rate(7.0);
    if h + m > 0 {
        println!(
            "hit rate  {:.1}% over the last 7 days ({h} hit / {m} miss in {n} runs, ~{:.0} min of rustc saved)",
            100.0 * h as f64 / (h + m) as f64,
            saved / 60.0
        );
        println!("          `justrust cache --misses` explains the misses");
    }
    let log = std::fs::read_to_string(root.join("gc.jsonl")).unwrap_or_default();
    if let Some(last) = log
        .lines()
        .last()
        .and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok())
    {
        println!(
            "evicted   {} entries ({}) {:.1}h ago in the background",
            last["evicted"],
            gb(last["bytes"].as_u64().unwrap_or(0)),
            (paths::now() - last["t"].as_f64().unwrap_or(0.0)) / 3600.0
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_cache_namespaces_are_removed_whole_fresh_ones_kept() {
        let ns = std::env::temp_dir().join(format!("jr-gc-ns-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ns);
        std::fs::create_dir_all(ns.join("fresh/depcache/o")).unwrap();
        std::fs::write(ns.join("not-a-dir"), b"").unwrap();
        assert_eq!(prune_namespaces(&ns, 3600.0), 0);
        assert_eq!(prune_namespaces(&ns.join("missing"), 0.0), 0);
        // Everything just written is newer than a negative age cutoff allows.
        assert_eq!(prune_namespaces(&ns, -3600.0), 1);
        assert!(!ns.join("fresh").exists());
        assert!(ns.join("not-a-dir").exists());
        std::fs::remove_dir_all(&ns).unwrap();
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("30G"), Some(30 << 30));
        assert_eq!(parse_size("512m"), Some(512 << 20));
        assert_eq!(parse_size("1.5GiB"), Some(3 << 29));
    }

    #[test]
    fn default_cap_follows_free_space() {
        const G: u64 = 1 << 30;
        assert_eq!(default_cap_for_disk(None), 30 * G);
        // Roomy disk (>= 200 GiB total): preserves 30-150 GiB behavior.
        assert_eq!(default_cap_for_disk(Some((40 * G, 500 * G))), 30 * G);
        assert_eq!(default_cap_for_disk(Some((400 * G, 1000 * G))), 100 * G);
        assert_eq!(default_cap_for_disk(Some((2500 * G, 4000 * G))), 150 * G);
        // Compact hosted guest disk (< 200 GiB total): bounded below 30 GiB
        // so depcache does not force EBS growth on small account disks.
        assert_eq!(default_cap_for_disk(Some((30 * G, 80 * G))), 8 * G);
        assert_eq!(default_cap_for_disk(Some((70 * G, 80 * G))), 16 * G);
        assert_eq!(default_cap_for_disk(Some((20 * G, 48 * G))), 6 * G);
        assert_eq!(default_cap_for_disk(Some((40 * G, 48 * G))), 48 * G / 5);
        assert!(disk_space(&std::env::temp_dir().join("no/such/dir")).is_some());
    }

    #[test]
    fn parses_plain_and_invalid_sizes() {
        assert_eq!(parse_size("1234"), Some(1234));
        assert_eq!(parse_size("0"), Some(0));
        assert_eq!(parse_size("lots"), None);
        assert_eq!(parse_size("5X"), None);
    }

    fn entry(root: &Path, key: &str, kb: usize, age_secs: f64) {
        let d = root.join("o").join(key);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("libx.rlib"), vec![7u8; kb * 1024]).unwrap();
        std::fs::write(d.join("meta.json"), b"{}").unwrap();
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs_f64(age_secs);
        std::fs::File::options()
            .append(true)
            .open(d.join("meta.json"))
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    #[cfg(unix)] // Open a directory handle to set its mtime.
    #[test]
    fn status_ignores_partial_stores_and_only_prune_collects_stale_ones() {
        let root = std::env::temp_dir().join(format!("jr-gc-status-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let old = root.join("o/.stale");
        let fresh = root.join("o/.fresh");
        for dir in [&old, &fresh] {
            std::fs::create_dir_all(dir.join("nested")).unwrap();
            std::fs::write(dir.join("nested/data"), b"partial").unwrap();
        }
        let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        std::fs::File::open(&old)
            .unwrap()
            .set_modified(stale)
            .unwrap();
        for _ in 0..2 {
            let s = scan(&root);
            assert!(s.entries.is_empty());
            assert_eq!(s.bytes, 0);
            for dir in [&old, &fresh] {
                assert_eq!(std::fs::read(dir.join("nested/data")).unwrap(), b"partial");
            }
        }
        assert_eq!(prune(&root, 0, false), Pruned::default());
        assert!(!old.exists());
        assert!(fresh.join("nested/data").is_file());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn exe_entry(root: &Path, key: &str, mb: usize, age_secs: f64) {
        entry(root, key, 1, age_secs);
        std::fs::write(root.join("o").join(key).join("app-0123abcd"), vec![9u8; mb << 20]).unwrap();
    }

    fn rules(cap: u64) -> Rules {
        Rules {
            cap,
            all: false,
            grace: 600.0,
            pin_floor: None,
            ephemeral_idle: 1800.0,
            ephemeral_cap: u64::MAX,
        }
    }

    fn tmp_root(name: &str) -> PathBuf {
        let r = std::env::temp_dir().join(format!("jr-gc-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&r);
        r
    }

    #[test]
    fn idle_link_outputs_expire_without_a_cap_and_libraries_stay() {
        let root = tmp_root("eph-idle");
        exe_entry(&root, "testbin-old", 9, 7200.0);
        exe_entry(&root, "testbin-new", 9, 60.0);
        entry(&root, "lib-old", 100, 7200.0);
        let p = prune_roots(&[&root], &rules(0));
        assert_eq!(p.entries, 1, "{p:?}");
        assert!(!root.join("o/testbin-old").exists(), "idle test binary expires");
        assert!(root.join("o/testbin-new").exists(), "a recent one is a revert away");
        assert!(root.join("o/lib-old").exists(), "libraries follow the cap, not the idle clock");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn link_outputs_over_their_own_cap_go_oldest_first() {
        let root = tmp_root("eph-cap");
        exe_entry(&root, "bin-a", 9, 1500.0);
        exe_entry(&root, "bin-b", 9, 900.0);
        exe_entry(&root, "bin-c", 9, 300.0);
        entry(&root, "lib", 50, 1500.0);
        let mut r = rules(0);
        r.ephemeral_cap = 20 << 20;
        let p = prune_roots(&[&root], &r);
        assert_eq!(p.entries, 1, "{p:?}");
        assert!(!root.join("o/bin-a").exists());
        assert!(root.join("o/bin-b").exists() && root.join("o/bin-c").exists() && root.join("o/lib").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn entries_pinned_by_a_running_build_survive_a_tiny_cap() {
        let root = tmp_root("pin");
        let now = paths::now();
        entry(&root, "before-build", 100, 7200.0);
        entry(&root, "restored-by-build", 100, 1800.0);
        let mut r = rules(1);
        r.grace = 0.0;
        // A build began an hour ago: whatever it used since is pinned.
        r.pin_floor = Some(now - 3600.0);
        prune_roots(&[&root], &r);
        assert!(!root.join("o/before-build").exists());
        assert!(root.join("o/restored-by-build").exists(), "pinned entry must not be evicted");
        // Without the lease the same collection takes it.
        r.pin_floor = None;
        prune_roots(&[&root], &r);
        assert!(!root.join("o/restored-by-build").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn one_cap_covers_two_roots_with_lru_across_both() {
        let (a, b) = (tmp_root("two-a"), tmp_root("two-b"));
        entry(&a, "a-old", 100, 9000.0);
        entry(&b, "b-mid", 100, 8000.0);
        entry(&a, "a-new", 100, 7000.0);
        entry(&b, "b-new", 100, 6000.0);
        // Room for two of four: the two oldest go, one from each root.
        let p = prune_roots(&[&a, &b], &rules(250 * 1024));
        assert_eq!(p.entries, 2, "{p:?}");
        assert!(!a.join("o/a-old").exists() && !b.join("o/b-mid").exists());
        assert!(a.join("o/a-new").exists() && b.join("o/b-new").exists());
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn a_partially_published_entry_is_never_an_inventory_entry_and_a_torn_tmp_is_collected() {
        // Publish writes `.<key>.tmp<pid>` then renames, so a build killed or
        // out of space midway leaves only a dot-dir: invisible to restore,
        // collected once stale.
        let root = tmp_root("torn");
        entry(&root, "good", 10, 100.0);
        let torn = root.join("o/.deadbeef.tmp4242");
        std::fs::create_dir_all(&torn).unwrap();
        std::fs::write(torn.join("libhalf.rlib"), b"half").unwrap();
        assert_eq!(scan(&root).entries.len(), 1);
        let stale = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        std::fs::File::open(&torn).unwrap().set_modified(stale).unwrap();
        prune_roots(&[&root], &rules(0));
        assert!(!torn.exists());
        assert!(root.join("o/good").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn evicts_least_recently_used_first_and_spares_recent() {
        let root = std::env::temp_dir().join(format!("jr-depcache-gc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        entry(&root, "old", 100, 86400.0);
        entry(&root, "mid", 100, 7200.0);
        entry(&root, "new", 100, 3600.0);
        entry(&root, "hot", 100, 1.0);
        std::fs::create_dir_all(root.join("m")).unwrap();
        let line = |out: &str| format!(r#"{{"inputs":[],"env":[],"out":"{out}"}}"#);
        std::fs::write(
            root.join("m/k.jsonl"),
            format!("{}\n{}\n", line("old"), line("new")),
        )
        .unwrap();

        let total = scan(&root).bytes;
        assert!(total >= 400 * 1024);
        // Under the cap: nothing happens.
        assert_eq!(prune(&root, total * 2, false), Pruned::default());
        // Cap of ~2.5 entries: low water 90% means two oldest go.
        let p = prune(&root, 250 * 1024, false);
        assert_eq!(p.entries, 2, "{p:?}");
        assert!(!root.join("o/old").exists() && !root.join("o/mid").exists());
        assert!(root.join("o/new").exists() && root.join("o/hot").exists());
        // Manifest lines for evicted entries are dropped.
        assert_eq!(
            std::fs::read_to_string(root.join("m/k.jsonl")).unwrap(),
            format!("{}\n", line("new"))
        );
        // Tiny cap: the recently used entry survives the grace period.
        prune(&root, 1, false);
        assert!(!root.join("o/new").exists());
        assert!(root.join("o/hot").exists());
        assert!(!root.join("m/k.jsonl").exists());
        assert_eq!(std::fs::read_dir(root.join("trash")).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}
