//! Disk governor shared by every cache justrust manages: the dependency
//! cache, the local-unit cache, private build slots and pool targets, run
//! records, and logs.
//!
//! Why one module. Each of those had its own size rule, measured in a
//! different unit, and none of them knew the volume was filling up:
//!
//! - `du` and `st_blocks` count every reflink clone in full. A build slot
//!   seeded by `cp -c` from the shared target dir "weighs" as much as the
//!   tree it cloned (measured: slot 0 at 40 GB logical, 6 GB private), so a
//!   budget in `du` bytes deletes cheap slots and misses the files that
//!   actually fill the disk.
//! - Executables and test binaries are published to the local-unit cache on
//!   every miss. They are hundreds of MB, almost never hit, and the cache
//!   keeps a clone of each until it is evicted, so the volume fills at the
//!   rate of one link output per build per agent while the cap is only
//!   checked every ten minutes.
//! - Nothing queued builds when the volume was nearly full. They started,
//!   wrote, and died with ENOSPC.
//!
//! What this module provides:
//!
//! - [`private_bytes`]: bytes freed if a file were deleted now (APFS
//!   `ATTR_CMNEXT_PRIVATESIZE`), falling back to allocated blocks.
//! - [`Lease`] and [`admit`]: every local build takes a lease before it
//!   writes. A lease reserves headroom, so N builds starting at once cannot
//!   each see the same free space, and a build that does not fit waits
//!   (after one reclaim attempt) instead of dying. Leases are flock files, so
//!   a killed build releases its lease and reservation with the process.
//! - [`pin_floor`]: store entries used since the oldest live lease began are
//!   not evicted by routine collection, so a running build never loses what
//!   it restored.
//! - [`reclaim`]: low-disk collection that orders victims by what they are
//!   worth (idle link outputs, then incremental dirs, then idle slots, then
//!   least recently used cache entries) and stops when the volume has room.
//! - [`retention`]: age and size bounds for run records and logs.
//!
//! Everything fails open: an error here means the build runs as before.

use crate::paths;
use serde::{Deserialize, Serialize};
use std::fs::{File, Metadata};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const GIB: u64 = 1 << 30;
pub const MIB: u64 = 1 << 20;

// ---------------------------------------------------------------------------
// Physical size
// ---------------------------------------------------------------------------

/// Bytes that deleting this file would free right now. On APFS a clone shares
/// extents with its source, and only the extents no other file references
/// count. Elsewhere (no clone information) allocated blocks.
pub fn private_bytes(path: &Path, md: &Metadata) -> u64 {
    if !md.is_file() {
        return 0;
    }
    let alloc = crate::sys::disk_bytes(md);
    if alloc == 0 {
        return 0;
    }
    #[cfg(target_os = "macos")]
    if let Some(p) = apfs_private_size(path) {
        return p.min(alloc);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = path;
    alloc
}

#[cfg(target_os = "macos")]
fn apfs_private_size(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    // Not exported by the libc crate. <sys/attr.h>.
    const ATTR_BIT_MAP_COUNT: u16 = 5;
    const ATTR_CMNEXT_PRIVATESIZE: u32 = 0x0000_0008;
    const FSOPT_NOFOLLOW: u32 = 0x0000_0001;
    const FSOPT_ATTR_CMN_EXTENDED: u32 = 0x0000_0020;
    let c = CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: a zeroed attrlist is valid; the buffer is a u32 length followed
    // by one off_t, which is exactly what ATTR_CMNEXT_PRIVATESIZE returns.
    unsafe {
        let mut al: libc::attrlist = std::mem::zeroed();
        al.bitmapcount = ATTR_BIT_MAP_COUNT;
        al.forkattr = ATTR_CMNEXT_PRIVATESIZE;
        let mut buf = [0u8; 12];
        let r = libc::getattrlist(
            c.as_ptr(),
            &mut al as *mut libc::attrlist as *mut libc::c_void,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            FSOPT_NOFOLLOW | FSOPT_ATTR_CMN_EXTENDED,
        );
        if r != 0 {
            return None;
        }
        let len = u32::from_ne_bytes(buf[0..4].try_into().ok()?);
        if (len as usize) < buf.len() {
            return None;
        }
        let p = i64::from_ne_bytes(buf[4..12].try_into().ok()?);
        u64::try_from(p).ok()
    }
}

/// Sum of [`private_bytes`] under `dir`, not following symlinks.
pub fn dir_private(dir: &Path) -> u64 {
    walk_sum(dir, &|p, m| private_bytes(p, m))
}

/// Sum of allocated blocks under `dir` (clones counted in full): the
/// worst-case footprint if everything sharing extents with it vanished.
pub fn dir_alloc(dir: &Path) -> u64 {
    walk_sum(dir, &|_, m| if m.is_file() { crate::sys::disk_bytes(m) } else { 0 })
}

fn walk_sum(dir: &Path, f: &dyn Fn(&Path, &Metadata) -> u64) -> u64 {
    let mut total = 0;
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        let Ok(m) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if m.is_dir() {
            total += walk_sum(&p, f);
        } else if m.is_file() {
            total += f(&p, &m);
        }
    }
    total
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

/// `JUSTRUST_DISK_GUARD=0` turns admission and low-disk reclaim off.
pub fn guard_enabled() -> bool {
    !matches!(
        std::env::var("JUSTRUST_DISK_GUARD").as_deref(),
        Ok("0" | "off" | "false" | "no")
    )
}

/// Free space a build must leave on the volume: `JUSTRUST_DISK_MIN_FREE`
/// (`20G`), else 1% of the volume clamped to 4-12 GiB.
pub fn min_free(total: u64) -> u64 {
    std::env::var("JUSTRUST_DISK_MIN_FREE")
        .ok()
        .and_then(|v| crate::depcache_gc::parse_size(&v))
        .unwrap_or_else(|| (total / 100).clamp(4 * GIB, 12 * GIB))
}

/// Store-wide cap on the dependency and local-unit caches together
/// (`JUSTRUST_STORE_MAX`, `0` = no cap). Unset: `JUSTRUST_DEPCACHE_MAX`
/// when the user already set it (it used to be the whole budget), else a
/// quarter of free disk, 30 to 150 GiB.
pub fn store_cap(free_total: Option<(u64, u64)>) -> u64 {
    if let Some(v) = std::env::var("JUSTRUST_STORE_MAX")
        .ok()
        .and_then(|v| crate::depcache_gc::parse_size(&v))
    {
        return v;
    }
    if let Some(v) = std::env::var("JUSTRUST_DEPCACHE_MAX")
        .ok()
        .and_then(|v| crate::depcache_gc::parse_size(&v))
    {
        return v;
    }
    crate::depcache_gc::default_cap_for_disk(free_total)
}

/// Executables and test binaries in the local-unit cache ("ephemeral"
/// entries): how long one may sit idle (`JUSTRUST_EPHEMERAL_IDLE_MIN`, 30)
/// and how much of them the store keeps (`JUSTRUST_EPHEMERAL_MAX`, a quarter
/// of `cap`, 2 to 8 GiB). They stay private to the agent that linked them
/// in every way except a revert, which is the only time they hit.
pub fn ephemeral_idle_secs() -> f64 {
    std::env::var("JUSTRUST_EPHEMERAL_IDLE_MIN")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .map_or(30.0 * 60.0, |m| m * 60.0)
}

pub fn ephemeral_cap(cap: u64) -> u64 {
    std::env::var("JUSTRUST_EPHEMERAL_MAX")
        .ok()
        .and_then(|v| crate::depcache_gc::parse_size(&v))
        .unwrap_or_else(|| {
            if cap == 0 {
                8 * GIB
            } else {
                (cap / 4).clamp(2 * GIB, 8 * GIB)
            }
        })
}

/// Headroom a command reserves while it runs, by subcommand
/// (`JUSTRUST_DISK_RESERVE`). Deliberately modest: dependencies come from the
/// store by clone, so a build mostly writes its own crates and link output.
pub fn reserve_for(subcommand: &str) -> u64 {
    if let Some(v) = std::env::var("JUSTRUST_DISK_RESERVE")
        .ok()
        .and_then(|v| crate::depcache_gc::parse_size(&v))
    {
        return v;
    }
    match subcommand {
        "check" | "c" | "clippy" => 2 * GIB,
        "test" | "t" => 4 * GIB,
        _ => 6 * GIB,
    }
}

// ---------------------------------------------------------------------------
// Leases
// ---------------------------------------------------------------------------

/// A running build's claim on the volume. One flock file per build under
/// `<home>/store/leases/`, held by the build process for its lifetime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LeaseRec {
    pub pid: u32,
    pub started: f64,
    /// Bytes this build may still write, counted against free space.
    pub reserve: u64,
    /// Device ids of the volumes it reserves on (0 where unknown).
    pub devs: Vec<u64>,
    pub cwd: String,
    pub cmd: String,
}

/// Dropping the lease releases the reservation and unpins cache entries.
pub struct Lease {
    _file: File,
    path: PathBuf,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn store_dir() -> Option<PathBuf> {
    paths::home().ok().map(|h| h.join("store"))
}

fn lease_dir() -> Option<PathBuf> {
    store_dir().map(|d| d.join("leases"))
}

/// Leases whose owner is still alive. A lease file nobody holds is a crashed
/// build's: removed here.
pub fn live_leases() -> Vec<LeaseRec> {
    live_leases_in(&lease_dir().unwrap_or_default())
}

fn live_leases_in(dir: &Path) -> Vec<LeaseRec> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "lease") {
            continue;
        }
        let Ok(f) = File::options().read(true).write(true).open(&p) else {
            continue;
        };
        if f.try_lock().is_ok() {
            // Nobody holds it: the build died without cleaning up.
            let _ = f.unlock();
            let _ = std::fs::remove_file(&p);
            continue;
        }
        if let Some(rec) = std::fs::read(&p)
            .ok()
            .and_then(|b| serde_json::from_slice::<LeaseRec>(&b).ok())
        {
            out.push(rec);
        }
    }
    out
}

/// Leases older than this stop pinning cache entries. A forgotten
/// `justrust run` server must not freeze collection forever.
const MAX_PIN_SECS: f64 = 6.0 * 3600.0;

/// Entries used at or after this time may be in use by a running build.
pub fn pin_floor() -> Option<f64> {
    pin_floor_of(&live_leases(), paths::now())
}

fn pin_floor_of(leases: &[LeaseRec], now: f64) -> Option<f64> {
    leases
        .iter()
        .map(|l| l.started)
        .filter(|s| now - s < MAX_PIN_SECS)
        .reduce(f64::min)
}

#[cfg(unix)]
fn dev_of(p: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    p.ancestors()
        .find_map(|a| std::fs::metadata(a).ok())
        .map_or(0, |m| m.dev())
}

#[cfg(not(unix))]
fn dev_of(_: &Path) -> u64 {
    0
}

/// Outcome of the admission arithmetic for one volume.
#[derive(Debug, PartialEq, Eq)]
pub enum Fit {
    Fits,
    Short { free: u64, need: u64 },
}

/// Does `reserve` fit on a volume with `free` of `total` bytes free, given
/// what other live leases already reserved there?
pub fn fit(free: u64, total: u64, reserved_by_others: u64, reserve: u64) -> Fit {
    let need = min_free(total)
        .saturating_add(reserved_by_others)
        .saturating_add(reserve);
    if free >= need {
        Fit::Fits
    } else {
        Fit::Short { free, need }
    }
}

fn check_all(
    dirs: &[PathBuf],
    others: &[LeaseRec],
    reserve: u64,
) -> Result<(), (PathBuf, u64, u64, usize)> {
    for d in dirs {
        let Some((free, total)) = crate::sys::disk_space(d.ancestors().find(|p| p.exists()).unwrap_or(d))
        else {
            continue;
        };
        let dev = dev_of(d);
        let held: Vec<&LeaseRec> = others.iter().filter(|l| l.devs.contains(&dev)).collect();
        let reserved: u64 = held.iter().map(|l| l.reserve).sum();
        if let Fit::Short { free, need } = fit(free, total, reserved, reserve) {
            return Err((d.clone(), free, need, held.len()));
        }
    }
    Ok(())
}

/// Wait budget before a build runs anyway (`JUSTRUST_DISK_WAIT_MIN`, 45).
fn wait_budget() -> Duration {
    Duration::from_secs_f64(
        std::env::var("JUSTRUST_DISK_WAIT_MIN")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(45.0)
            .max(0.0)
            * 60.0,
    )
}

/// Take a lease for a local build in `cwd`, waiting while the volumes it
/// writes to are too full. One reclaim runs first. After the wait budget the
/// build proceeds with a warning: queueing must never be worse than before.
/// `None` when the guard is off or the lease directory is unusable.
pub fn admit(cwd: &Path, cmd: &str, reserve: u64, err: &mut dyn Write) -> Option<Lease> {
    if !guard_enabled() {
        return None;
    }
    let dir = lease_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let mut write_dirs = vec![cwd.to_path_buf()];
    if let Ok(h) = paths::home() {
        write_dirs.push(h);
    }
    let mut devs: Vec<u64> = write_dirs.iter().map(|d| dev_of(d)).collect();
    devs.sort_unstable();
    devs.dedup();

    let started = Instant::now();
    let budget = wait_budget();
    let mut reclaimed = false;
    let mut last_reclaim = Instant::now();
    let mut last_note = Instant::now() - Duration::from_secs(60);
    loop {
        // The gate makes check-then-create atomic across processes.
        let gate = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("admit.lock"))
            .ok()?;
        let gate_deadline = Instant::now() + Duration::from_secs(10);
        while gate.try_lock().is_err() {
            if Instant::now() > gate_deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let others = live_leases_in(&dir);
        match check_all(&write_dirs, &others, reserve) {
            Ok(()) => return create_lease(&dir, cwd, cmd, reserve, devs),
            Err((path, free, need, holders)) => {
                drop(gate);
                if !reclaimed {
                    reclaimed = true;
                    let _ = writeln!(
                        err,
                        "justrust: only {} free on {}, need {} to start: reclaiming cache space",
                        gb(free),
                        path.display(),
                        gb(need)
                    );
                    reclaim(need.saturating_sub(free));
                    continue;
                }
                if started.elapsed() >= budget {
                    let _ = writeln!(
                        err,
                        "justrust: disk still short after {:.0} min ({} free, {} wanted): \
                         building anyway (JUSTRUST_DISK_WAIT_MIN)",
                        started.elapsed().as_secs_f64() / 60.0,
                        gb(free),
                        gb(need)
                    );
                    let gate2 = File::options()
                        .create(true)
                        .truncate(false)
                        .write(true)
                        .open(dir.join("admit.lock"))
                        .ok();
                    let _ = gate2.as_ref().map(|g| g.try_lock());
                    return create_lease(&dir, cwd, cmd, reserve, devs);
                }
                if last_note.elapsed() >= Duration::from_secs(30) {
                    last_note = Instant::now();
                    let _ = writeln!(
                        err,
                        "justrust: waiting for disk space on {}: {} free, {} wanted, \
                         {} other build(s) hold reservations (queued {:.0}s)",
                        path.display(),
                        gb(free),
                        gb(need),
                        holders,
                        started.elapsed().as_secs_f64()
                    );
                }
                std::thread::sleep(Duration::from_secs(3));
                // A reclaim that found nothing is retried now and then:
                // other builds finishing and agents deleting worktrees free
                // space without telling us.
                // After half a minute of waiting the thresholds go to zero.
                if started.elapsed() >= Duration::from_secs(30)
                    && last_reclaim.elapsed() >= Duration::from_secs(30)
                {
                    last_reclaim = Instant::now();
                    reclaim_scaled(need.saturating_sub(free), 0.0);
                }
            }
        }
    }
}

fn create_lease(
    dir: &Path,
    cwd: &Path,
    cmd: &str,
    reserve: u64,
    devs: Vec<u64>,
) -> Option<Lease> {
    let path = dir.join(format!("{}-{}.lease", std::process::id(), nonce()));
    let mut f = File::options()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .ok()?;
    f.try_lock().ok()?;
    let rec = LeaseRec {
        pid: std::process::id(),
        started: paths::now(),
        reserve,
        devs,
        cwd: cwd.to_string_lossy().into_owned(),
        cmd: cmd.to_owned(),
    };
    f.write_all(&serde_json::to_vec(&rec).ok()?).ok()?;
    Some(Lease { _file: f, path })
}

fn nonce() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

pub fn gb(b: u64) -> String {
    format!("{:.1} GB", b as f64 / GIB as f64)
}

// ---------------------------------------------------------------------------
// Roots registry: private build trees the reclaimer may prune
// ---------------------------------------------------------------------------

/// Remember a directory of slots (`<target>/justrust-slots`) or pool targets
/// so low-disk reclaim finds trees of workspaces it is not running in.
pub fn register_root(kind: &str, root: &Path) {
    let Some(dir) = store_dir().map(|d| d.join("roots")) else {
        return;
    };
    let mut h = crate::depcache::H128::new();
    h.field(kind.as_bytes());
    h.field(root.as_os_str().as_encoded_bytes());
    let file = dir.join(format!("{kind}-{}", h.hex()));
    if file.exists() {
        return;
    }
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&file, root.as_os_str().as_encoded_bytes());
}

/// Registered roots of `kind` that still exist. Stale registrations (the
/// workspace was deleted) are dropped.
pub fn registered_roots(kind: &str) -> Vec<PathBuf> {
    let Some(dir) = store_dir().map(|d| d.join("roots")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        if !e.file_name().to_string_lossy().starts_with(&format!("{kind}-")) {
            continue;
        }
        let Ok(text) = std::fs::read(e.path()) else {
            continue;
        };
        let p = PathBuf::from(std::ffi::OsString::from(String::from_utf8_lossy(&text).into_owned()));
        if p.is_dir() {
            out.push(p);
        } else {
            let _ = std::fs::remove_file(e.path());
        }
    }
    out.sort();
    out.dedup();
    out
}

// ---------------------------------------------------------------------------
// Low-disk reclaim
// ---------------------------------------------------------------------------

/// Free about `want` bytes, cheapest victims first. Returns bytes freed by
/// what it removed (private bytes, so clones that free nothing do not count).
/// Safe to run beside builds: store entries leave by atomic rename, slots and
/// pool targets only when their lock is free.
pub fn reclaim(want: u64) -> u64 {
    reclaim_scaled(want, 1.0)
}

/// `reclaim` with every idle threshold multiplied by `idle_scale`. A build
/// that has waited for room passes 0: locks already protect what is in use,
/// so the thresholds only protect warmth, and warmth is worth less than a
/// build that cannot start.
pub fn reclaim_scaled(want: u64, idle_scale: f64) -> u64 {
    let mut freed = 0;
    let roots: Vec<PathBuf> = [
        crate::depcache::cache_dir(),
        crate::localcache::cache_dir(),
    ]
    .into_iter()
    .flatten()
    .filter(|r| r.join("o").is_dir())
    .collect();
    // 1. Link outputs nobody has touched for a while: the cheapest bytes.
    freed += crate::depcache_gc::reclaim_ephemeral(&roots, want, idle_scale);
    // 2. Incremental state of idle slots, then idle slots themselves.
    if freed < want {
        for root in registered_roots("slots") {
            freed += crate::slots::reclaim(&root, want.saturating_sub(freed), idle_scale);
            if freed >= want {
                break;
            }
        }
    }
    // 2b. Pool targets: idle ones' incremental dirs, then idle targets.
    if freed < want
        && let Some(pool) = crate::pool::pool_root().filter(|p| p.is_dir())
    {
        freed += crate::pool::reclaim(&pool, want - freed, idle_scale);
    }
    // 3. Least recently used cache entries that free real space.
    if freed < want {
        freed += crate::depcache_gc::reclaim_lru(&roots, want - freed, idle_scale);
    }
    freed
}

// ---------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------

pub mod retention {
    use super::*;

    /// Run records: newest `RUNS_KEEP`, none older than `RUNS_MAX_AGE_DAYS`,
    /// and at most `RUNS_MAX_BYTES` together. Tunable by
    /// `JUSTRUST_RUNS_KEEP`, `JUSTRUST_RUNS_MAX_DAYS`, `JUSTRUST_RUNS_MAX`.
    const RUNS_KEEP: usize = 1500;
    const RUNS_MAX_AGE_DAYS: f64 = 14.0;
    const RUNS_MAX_BYTES: u64 = GIB;
    /// Logs that only ever append keep their newest bytes.
    const LOG_KEEP_BYTES: u64 = 16 * MIB;
    const JOURNAL_KEEP_BYTES: u64 = 64 * MIB;
    const INTERVAL_SECS: f64 = 3600.0;

    #[derive(Debug, Default, PartialEq)]
    pub struct Report {
        pub runs_removed: usize,
        pub runs_bytes: u64,
        pub logs_trimmed: usize,
        pub hints_removed: usize,
    }

    fn env_num<T: std::str::FromStr>(k: &str, d: T) -> T {
        std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
    }

    /// Hourly, detached from builds by the caller.
    pub fn due() -> bool {
        let Some(stamp) = super::store_dir().map(|d| d.join("retention.stamp")) else {
            return false;
        };
        match std::fs::metadata(&stamp) {
            Ok(m) if paths::now() - crate::sys::mtime_f64(&m) < INTERVAL_SECS => false,
            _ => {
                let _ = std::fs::create_dir_all(stamp.parent().unwrap());
                std::fs::write(&stamp, b"").is_ok()
            }
        }
    }

    pub fn run() -> Report {
        let mut r = Report::default();
        let Ok(home) = paths::home() else {
            return r;
        };
        let keep = env_num("JUSTRUST_RUNS_KEEP", RUNS_KEEP);
        let max_age = env_num("JUSTRUST_RUNS_MAX_DAYS", RUNS_MAX_AGE_DAYS) * 86400.0;
        let max_bytes = std::env::var("JUSTRUST_RUNS_MAX")
            .ok()
            .and_then(|v| crate::depcache_gc::parse_size(&v))
            .unwrap_or(RUNS_MAX_BYTES);
        let (n, b) = prune_runs(&home.join("runs"), keep, max_age, max_bytes, paths::now());
        r.runs_removed = n;
        r.runs_bytes = b;
        if n > 0 {
            trim_index(&home.join("index.jsonl"), &home.join("runs"));
        }
        for (path, keep) in [
            (home.join("index.jsonl"), 64 * MIB),
            (home.join("split/edits.jsonl"), LOG_KEEP_BYTES),
            (home.join("cache/depcache/gc.jsonl"), LOG_KEEP_BYTES),
            (home.join("cache/localcache/finish.jsonl"), LOG_KEEP_BYTES),
            (home.join("cache/depcache/peer-journal.jsonl"), JOURNAL_KEEP_BYTES),
        ] {
            if trim_log(&path, keep) {
                r.logs_trimmed += 1;
            }
        }
        // Churn and fact snapshots are recomputed from recorded runs.
        let (pool_removed, pool_bytes) = crate::pool::sweep_default();
        r.runs_removed += pool_removed;
        r.runs_bytes += pool_bytes;
        r.hints_removed = remove_older(&home.join("split"), 30.0 * 86400.0, |n| {
            n.starts_with("churn-") || n.starts_with("facts-")
        });
        r
    }

    /// Remove run directories by the three bounds, oldest first. A run dir
    /// is "old" by the mtime of its directory. Returns (removed, bytes).
    pub fn prune_runs(
        runs: &Path,
        keep: usize,
        max_age: f64,
        max_bytes: u64,
        now: f64,
    ) -> (usize, u64) {
        let mut dirs: Vec<(f64, PathBuf, u64)> = std::fs::read_dir(runs)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let m = e.metadata().ok()?;
                m.is_dir()
                    .then(|| (crate::sys::mtime_f64(&m), e.path(), dir_alloc(&e.path())))
            })
            .collect();
        // Newest first.
        dirs.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut total: u64 = dirs.iter().map(|d| d.2).sum();
        let (mut removed, mut bytes) = (0, 0);
        for (i, (mtime, path, size)) in dirs.iter().enumerate().rev() {
            let over_count = i >= keep;
            let over_age = now - mtime > max_age;
            let over_size = total > max_bytes && i >= 50;
            if !(over_count || over_age || over_size) {
                // Oldest-first walk: nothing newer is over any bound either,
                // except by size, which is cumulative.
                if total <= max_bytes {
                    break;
                }
                continue;
            }
            if std::fs::remove_dir_all(path).is_ok() {
                removed += 1;
                bytes += size;
                total = total.saturating_sub(*size);
            }
        }
        (removed, bytes)
    }

    /// Drop index lines whose run directory is gone.
    fn trim_index(index: &Path, runs: &Path) {
        let Ok(text) = std::fs::read_to_string(index) else {
            return;
        };
        let keep: Vec<&str> = text
            .lines()
            .filter(|l| {
                serde_json::from_str::<serde_json::Value>(l)
                    .ok()
                    .and_then(|v| v["id"].as_str().map(|id| runs.join(id).is_dir()))
                    .unwrap_or(false)
            })
            .collect();
        if keep.len() == text.lines().count() {
            return;
        }
        let tmp = index.with_extension(format!("tmp{}", std::process::id()));
        let mut body = keep.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        if std::fs::write(&tmp, body).is_ok() && std::fs::rename(&tmp, index).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Keep the newest `keep` bytes of an append-only log, from a line
    /// boundary. Returns whether the file was cut. The rewrite is atomic; a
    /// line appended during it can be lost, which these logs tolerate.
    pub fn trim_log(path: &Path, keep: u64) -> bool {
        use std::io::{Read, Seek, SeekFrom};
        let Ok(mut f) = File::open(path) else {
            return false;
        };
        let Ok(len) = f.metadata().map(|m| m.len()) else {
            return false;
        };
        if len <= keep + keep / 4 {
            return false;
        }
        if f.seek(SeekFrom::Start(len - keep)).is_err() {
            return false;
        }
        let mut tail = Vec::with_capacity(keep as usize);
        if f.read_to_end(&mut tail).is_err() {
            return false;
        }
        let start = tail.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1);
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, &tail[start..]).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return false;
        }
        if std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return false;
        }
        true
    }

    fn remove_older(dir: &Path, age: f64, pick: impl Fn(&str) -> bool) -> usize {
        let now = paths::now();
        let mut n = 0;
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !pick(&name) {
                continue;
            }
            if let Ok(m) = e.metadata()
                && now - crate::sys::mtime_f64(&m) > age
                && std::fs::remove_file(e.path()).is_ok()
            {
                n += 1;
            }
        }
        n
    }
}

// ---------------------------------------------------------------------------
// `justrust store`
// ---------------------------------------------------------------------------

/// `justrust store [--gc] [--reclaim <GB>] [--retention]`.
pub fn command(gc: bool, reclaim_gb: Option<f64>, run_retention: bool) -> anyhow::Result<()> {
    if gc {
        for root in [
            crate::depcache::cache_dir(),
            crate::localcache::cache_dir(),
        ]
        .into_iter()
        .flatten()
        {
            if root.join("o").is_dir() {
                let p = crate::depcache_gc::prune(&root, crate::depcache_gc::root_cap(&root), false);
                println!("{}: evicted {} entries, {}", root.display(), p.entries, gb(p.bytes));
            }
        }
    }
    if let Some(g) = reclaim_gb {
        let freed = reclaim((g * GIB as f64) as u64);
        println!("reclaim: freed {}", gb(freed));
    }
    if run_retention {
        println!("retention: {:?}", retention::run());
    }
    let home = paths::home()?;
    let probe = home.clone();
    if let Some((free, total)) = crate::sys::disk_space(&probe.ancestors().find(|p| p.exists()).unwrap_or(&probe)) {
        println!(
            "volume    {} free of {}, builds keep {} free (JUSTRUST_DISK_MIN_FREE){}",
            gb(free),
            gb(total),
            gb(min_free(total)),
            if guard_enabled() { "" } else { "  [guard OFF]" }
        );
        println!("store cap {} (JUSTRUST_STORE_MAX)", {
            let c = store_cap(Some((free, total)));
            if c == 0 { "none".into() } else { gb(c) }
        });
    }
    let leases = live_leases();
    println!("leases    {} running build(s)", leases.len());
    for l in &leases {
        println!(
            "          pid {} reserves {} since {:.0}s ago: {}",
            l.pid,
            gb(l.reserve),
            paths::now() - l.started,
            l.cmd
        );
    }
    for (name, root) in [
        ("depcache  ", crate::depcache::cache_dir()),
        ("localcache", crate::localcache::cache_dir()),
    ] {
        if let Some(root) = root.filter(|r| r.join("o").is_dir()) {
            let s = crate::depcache_gc::scan_phys(&root);
            println!(
                "{name} {} allocated, {} private to it, {} entries ({} link outputs, {} allocated)",
                gb(s.alloc),
                gb(s.private),
                s.entries,
                s.ephemeral,
                gb(s.ephemeral_alloc)
            );
        }
    }
    for root in registered_roots("slots") {
        let ns = crate::slots::root_summary(&root);
        println!("slots     {}  {}", root.display(), ns);
    }
    if let Some(pool) = crate::pool::pool_root().filter(|p| p.is_dir()) {
        println!("pool      {}  {}", pool.display(), crate::pool::summary(&pool));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("jr-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn private_bytes_of_a_plain_file_is_its_allocation_and_a_clone_is_nearly_free() {
        let d = tmp("private");
        let a = d.join("a");
        std::fs::write(&a, vec![1u8; 4 << 20]).unwrap();
        // APFS reports private size from allocated extents: flush first.
        std::fs::File::open(&a).unwrap().sync_all().unwrap();
        let ma = std::fs::metadata(&a).unwrap();
        assert!(private_bytes(&a, &ma) >= 4 << 20, "{}", private_bytes(&a, &ma));
        assert_eq!(private_bytes(&d, &std::fs::metadata(&d).unwrap()), 0);
        let b = d.join("b");
        if crate::depcache::reflink_or_copy(&a, &b) && cfg!(target_os = "macos") {
            // On APFS the clone shares every extent with `a` (when /tmp is
            // APFS; a plain copy on other volumes is private).
            let mb = std::fs::metadata(&b).unwrap();
            let p = private_bytes(&b, &mb);
            assert!(p <= crate::sys::disk_bytes(&mb));
        }
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn fit_counts_other_reservations_and_the_free_floor() {
        let total = 1000 * GIB; // floor = 10 GiB
        assert_eq!(min_free(total), 10 * GIB);
        assert_eq!(fit(100 * GIB, total, 0, 4 * GIB), Fit::Fits);
        assert_eq!(fit(14 * GIB, total, 0, 4 * GIB), Fit::Fits);
        assert!(matches!(fit(13 * GIB, total, 0, 4 * GIB), Fit::Short { .. }));
        // Three builds already hold 4 GiB each: a fourth does not fit in 20.
        assert!(matches!(fit(20 * GIB, total, 12 * GIB, 4 * GIB), Fit::Short { need, .. } if need == 26 * GIB));
        assert_eq!(fit(26 * GIB, total, 12 * GIB, 4 * GIB), Fit::Fits);
    }

    #[test]
    fn min_free_scales_with_the_volume_within_bounds() {
        assert_eq!(min_free(100 * GIB), 4 * GIB);
        assert_eq!(min_free(500 * GIB), 5 * GIB);
        assert_eq!(min_free(1000 * GIB), 10 * GIB);
        assert_eq!(min_free(4000 * GIB), 12 * GIB);
    }

    #[test]
    fn a_lease_is_live_while_held_and_gone_after_drop_or_crash() {
        let d = tmp("lease");
        let l = create_lease(&d, Path::new("/ws"), "test", 3 * GIB, vec![1]).unwrap();
        let live = live_leases_in(&d);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].reserve, 3 * GIB);
        assert_eq!(live[0].cmd, "test");
        drop(l);
        assert!(live_leases_in(&d).is_empty());
        // A lease file nobody holds (killed build) is reaped, not counted.
        std::fs::write(
            d.join("999-1.lease"),
            serde_json::to_vec(&LeaseRec {
                pid: 999,
                started: 0.0,
                reserve: GIB,
                devs: vec![1],
                cwd: String::new(),
                cmd: String::new(),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(live_leases_in(&d).is_empty());
        assert!(!d.join("999-1.lease").exists());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn pins_start_at_the_oldest_recent_lease_and_ignore_forgotten_ones() {
        let rec = |started| LeaseRec {
            pid: 1,
            started,
            reserve: 0,
            devs: vec![],
            cwd: String::new(),
            cmd: String::new(),
        };
        let now = 100_000.0;
        assert_eq!(pin_floor_of(&[], now), None);
        assert_eq!(pin_floor_of(&[rec(now - 60.0), rec(now - 600.0)], now), Some(now - 600.0));
        // A lease older than the pin horizon does not freeze collection.
        assert_eq!(pin_floor_of(&[rec(now - 7.0 * 3600.0)], now), None);
        assert_eq!(
            pin_floor_of(&[rec(now - 7.0 * 3600.0), rec(now - 10.0)], now),
            Some(now - 10.0)
        );
    }

    #[test]
    fn registered_roots_drop_deleted_workspaces() {
        let home = tmp("roots");
        let ws = home.join("ws/justrust-slots");
        std::fs::create_dir_all(&ws).unwrap();
        // register_root/registered_roots use JUSTRUST_HOME; exercise the
        // file format directly to avoid process-wide env mutation.
        let dir = home.join("store/roots");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("slots-a"), ws.as_os_str().as_encoded_bytes()).unwrap();
        std::fs::write(dir.join("slots-b"), home.join("gone").as_os_str().as_encoded_bytes()).unwrap();
        let mut seen = Vec::new();
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = PathBuf::from(String::from_utf8(std::fs::read(e.path()).unwrap()).unwrap());
            if p.is_dir() {
                seen.push(p);
            }
        }
        assert_eq!(seen, vec![ws]);
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn trim_log_keeps_the_tail_from_a_line_boundary() {
        let d = tmp("trim");
        let p = d.join("log.jsonl");
        let lines: String = (0..1000).map(|i| format!("{{\"i\":{i}}}\n")).collect();
        std::fs::write(&p, &lines).unwrap();
        assert!(!retention::trim_log(&p, 1 << 20), "under the bound: untouched");
        assert!(retention::trim_log(&p, 2000));
        let kept = std::fs::read_to_string(&p).unwrap();
        assert!(kept.len() <= 2000 && kept.len() > 1000);
        assert!(kept.starts_with("{\"i\":"), "starts on a whole line: {kept:?}");
        assert!(kept.ends_with("{\"i\":999}\n"));
        assert!(kept.lines().all(|l| serde_json::from_str::<serde_json::Value>(l).is_ok()));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn prune_runs_applies_count_age_and_size_bounds_oldest_first() {
        let d = tmp("runs");
        let now = paths::now();
        let mk = |name: &str, age: f64, kb: usize| {
            let r = d.join(name);
            std::fs::create_dir_all(&r).unwrap();
            std::fs::write(r.join("output.jsonl"), vec![b'x'; kb * 1024]).unwrap();
            let t = std::time::SystemTime::now() - Duration::from_secs_f64(age);
            File::open(&r).unwrap().set_modified(t).unwrap();
        };
        mk("old", 20.0 * 86400.0, 10);
        mk("a", 3600.0, 10);
        mk("b", 2400.0, 10);
        mk("c", 1200.0, 10);
        mk("d", 60.0, 10);
        // Age: only `old` goes.
        assert_eq!(retention::prune_runs(&d, 100, 14.0 * 86400.0, u64::MAX, now).0, 1);
        assert!(!d.join("old").exists() && d.join("a").exists());
        // Count: keep the newest two.
        assert_eq!(retention::prune_runs(&d, 2, 14.0 * 86400.0, u64::MAX, now).0, 2);
        assert!(!d.join("a").exists() && !d.join("b").exists());
        assert!(d.join("c").exists() && d.join("d").exists());
        std::fs::remove_dir_all(d).unwrap();
    }
}
