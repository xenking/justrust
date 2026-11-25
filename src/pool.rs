//! Pool targets: one managed home for every private cargo target dir.
//!
//! A target dir cannot be shared between checkouts: Cargo hashes workspace
//! units without the checkout path, so two worktrees writing one dir
//! overwrite each other's artifacts (FINDINGS 2026-10-10). What can be shared
//! is the content-addressed store (dependencies, and portable local units).
//! The private part is the problem when it lives in `<worktree>/target`: it
//! is invisible to every budget, outlives the worktree, and costs a cold
//! build's worth of disk (17 GB each on the measured machine).
//!
//! ```text
//! ~/.justrust/pool/<key>-<name>/        one per checkout (key = hash of its root)
//!   .pool.json                          root, last use
//!   default/  verify/  rollout/ ...     one target dir per purpose
//!   default.lock                        flock held by a build using `default/`
//! ```
//!
//! `justrust target-dir [--name N]` prints (and creates) the dir for the
//! current checkout. With `JUSTRUST_POOL=1` a build that did not choose a
//! target uses `default` automatically. The sweeper removes a checkout's
//! dirs when the checkout is gone or untouched for `JUSTRUST_POOL_MAX_DAYS`
//! (14), and low-disk reclaim drops idle dirs' incremental state first.
//! A dir whose lock is held is never touched.

use crate::paths;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};

/// `JUSTRUST_POOL`: `1`/`link` makes `<workspace>/target` a symlink into the
/// pool (every tool that expects `./target` keeps working, the bytes are
/// managed); `dir` passes `--target-dir <pool dir>` instead.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Mode {
    Off,
    Link,
    Dir,
}

fn mode() -> Mode {
    match std::env::var("JUSTRUST_POOL").as_deref() {
        Ok("1" | "on" | "true" | "yes" | "link") => Mode::Link,
        Ok("dir") => Mode::Dir,
        _ => Mode::Off,
    }
}

pub fn pool_root() -> Option<PathBuf> {
    paths::home().ok().map(|h| h.join("pool"))
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct Meta {
    root: String,
    last_used: f64,
}

/// The checkout a build belongs to: the git toplevel, else the directory.
fn checkout_root(cwd: &Path) -> PathBuf {
    git_top(cwd).unwrap_or_else(|| cwd.to_path_buf())
}

fn git_top(cwd: &Path) -> Option<PathBuf> {
    std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
}

fn key_dir(pool: &Path, root: &Path) -> PathBuf {
    let mut h = crate::depcache::H128::new();
    h.field(root.as_os_str().as_encoded_bytes());
    let base: String = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(32)
        .collect();
    pool.join(format!("{}-{base}", &h.hex()[..12]))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 48
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Create (if needed), stamp, and return the target dir for `name` in the
/// checkout containing `cwd`.
pub fn resolve(cwd: &Path, name: &str) -> Option<PathBuf> {
    if !valid_name(name) {
        return None;
    }
    let pool = pool_root()?;
    let dir = resolve_in(&pool, &checkout_root(cwd), name)?;
    crate::store::register_root("pool", &pool);
    Some(dir)
}

fn resolve_in(pool: &Path, root: &Path, name: &str) -> Option<PathBuf> {
    resolve_at(&key_dir(pool, root), &root.to_string_lossy(), name)
}

/// A pool dir under an explicit key instead of a checkout: for builds whose
/// checkout path changes every run but whose artifacts should be reused
/// (the paired acceptance smoke). No checkout to disappear, so the sweeper
/// only ages it out.
fn resolve_keyed(pool: &Path, key: &str, name: &str) -> Option<PathBuf> {
    if !valid_name(key) {
        return None;
    }
    resolve_at(&pool.join(format!("key-{key}")), "", name)
}

fn resolve_at(kd: &Path, root: &str, name: &str) -> Option<PathBuf> {
    let dir = kd.join(name);
    std::fs::create_dir_all(&dir).ok()?;
    let meta = Meta {
        root: root.to_owned(),
        last_used: paths::now(),
    };
    let tmp = kd.join(format!(".pool.json.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(&meta).ok()?).ok()?;
    std::fs::rename(&tmp, kd.join(".pool.json")).ok()?;
    Some(dir)
}

fn user_chose_target(args: &[OsString]) -> bool {
    let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    set("CARGO_TARGET_DIR")
        || set("CARGO_BUILD_TARGET_DIR")
        || args
            .iter()
            .map(|a| a.to_string_lossy())
            .take_while(|a| a != "--")
            .any(|a| a.contains("target-dir"))
}

/// The cargo workspace root containing `cwd`: the highest ancestor (up to
/// the git toplevel) whose Cargo.toml has a `[workspace]`, else the nearest
/// ancestor with a Cargo.toml.
fn workspace_root(cwd: &Path) -> Option<PathBuf> {
    let top = git_top(cwd);
    let mut nearest = None;
    let mut best = None;
    for d in cwd.ancestors() {
        if let Ok(text) = std::fs::read_to_string(d.join("Cargo.toml")) {
            nearest.get_or_insert_with(|| d.to_path_buf());
            if text.lines().any(|l| l.trim() == "[workspace]") {
                best = Some(d.to_path_buf());
            }
        }
        if top.as_deref() == Some(d) {
            break;
        }
    }
    best.or(nearest)
}

/// Make `<workspace>/target` a symlink to the pool's `default` for this
/// checkout. An existing real `target` dir is moved into the pool (same
/// volume, one rename) so its warm state is kept and from then on managed.
/// Returns the pool dir. Never replaces a symlink the user made elsewhere.
fn link_target(cwd: &Path) -> Option<PathBuf> {
    let root = workspace_root(cwd)?;
    let pool = pool_root()?;
    let link = root.join("target");
    let kd = key_dir(&pool, &root);
    let dest = kd.join("default");
    match std::fs::symlink_metadata(&link) {
        Ok(m) if m.file_type().is_symlink() => {
            // Ours already, or deliberately elsewhere: leave it.
            return if std::fs::read_link(&link).ok()? == dest {
                resolve_in(&pool, &root, "default")
            } else {
                None
            };
        }
        Ok(m) if m.is_dir() => {
            std::fs::create_dir_all(&kd).ok()?;
            if dest.exists() && std::fs::read_dir(&dest).ok()?.next().is_some() {
                return None;
            }
            let _ = std::fs::remove_dir(&dest);
            std::fs::rename(&link, &dest).ok()?;
        }
        Ok(_) => return None,
        Err(_) => {}
    }
    let dir = resolve_in(&pool, &root, "default")?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(&dir, &link).ok()?;
    #[cfg(not(unix))]
    return None;
    crate::store::register_root("pool", &pool);
    Some(dir)
}

/// With `JUSTRUST_POOL` set, give a build that chose no target dir the
/// checkout's pool `default`. Any failure leaves the arguments alone.
pub fn with_target(args: Vec<OsString>) -> Vec<OsString> {
    if mode() == Mode::Off || user_chose_target(&args) {
        return args;
    }
    let Some(sub) = crate::record::subcommand(&args) else {
        return args;
    };
    if !matches!(
        sub.as_str(),
        "check" | "c" | "test" | "t" | "build" | "b" | "clippy" | "run" | "r" | "bench" | "doc"
    ) {
        return args;
    }
    if mode() == Mode::Link {
        // The symlink is the whole change: cargo resolves it, and the
        // pool lock is taken on the pool dir behind it.
        if let Ok(cwd) = std::env::current_dir() {
            let _ = link_target(&cwd);
        }
        return args;
    }
    let Some(dir) = std::env::current_dir()
        .ok()
        .and_then(|cwd| resolve(&cwd, "default"))
    else {
        return args;
    };
    let mut args = args;
    if let Some(at) = args.iter().position(|a| a.to_string_lossy() == sub) {
        args.insert(at + 1, OsString::from("--target-dir"));
        args.insert(at + 2, dir.into_os_string());
    }
    args
}

/// A running build's hold on a pool target. Dropping it releases the lock.
pub struct Hold {
    _lock: File,
}

fn try_lock(path: &Path) -> Option<File> {
    let f = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()?;
    f.try_lock().is_ok().then_some(f)
}

/// The target dir a command will write, when it is one of ours.
fn pool_target_of(args: &[OsString], pool: &Path) -> Option<PathBuf> {
    let mut it = args.iter().map(|a| a.to_string_lossy().into_owned());
    let mut found = None;
    while let Some(a) = it.next() {
        if a == "--" {
            break;
        }
        if a == "--target-dir" {
            found = it.next();
        } else if let Some(v) = a.strip_prefix("--target-dir=") {
            found = Some(v.to_owned());
        }
    }
    let found = found.or_else(|| {
        std::env::var("CARGO_TARGET_DIR")
            .ok()
            .filter(|v| !v.is_empty())
    })?;
    let p = PathBuf::from(found);
    // <pool>/<key>/<name>, possibly with a trailing slash.
    let rel = p.strip_prefix(pool).ok()?;
    (rel.components().count() == 2).then_some(p)
}

/// Hold the pool target `args` writes to, if it is one, and stamp its last
/// use. A held target is never swept or reclaimed.
pub fn hold(args: &[OsString]) -> Option<Hold> {
    let pool = pool_root()?;
    let dir = pool_target_of(args, &pool).or_else(|| {
        // Link mode: `<workspace>/target` points into the pool.
        let cwd = std::env::current_dir().ok()?;
        let link = workspace_root(&cwd)?.join("target");
        let dest = std::fs::read_link(&link).ok()?;
        let rel = dest.strip_prefix(&pool).ok()?;
        (rel.components().count() == 2).then_some(dest)
    })?;
    let name = dir.file_name()?.to_string_lossy().into_owned();
    let kd = dir.parent()?;
    std::fs::create_dir_all(&dir).ok()?;
    if let Ok(text) = std::fs::read(kd.join(".pool.json"))
        && let Ok(mut m) = serde_json::from_slice::<Meta>(&text)
    {
        m.last_used = paths::now();
        let _ = std::fs::write(kd.join(".pool.json"), serde_json::to_vec(&m).unwrap_or_default());
    }
    Some(Hold {
        _lock: try_lock(&kd.join(format!("{name}.lock")))?,
    })
}

/// (checkout dir, target name, target dir, meta) for every pool target.
fn targets(pool: &Path) -> Vec<(PathBuf, String, PathBuf, Meta)> {
    let mut out = Vec::new();
    for kd in std::fs::read_dir(pool).into_iter().flatten().flatten() {
        let kd = kd.path();
        let Ok(text) = std::fs::read(kd.join(".pool.json")) else {
            continue;
        };
        let Ok(meta) = serde_json::from_slice::<Meta>(&text) else {
            continue;
        };
        for t in std::fs::read_dir(&kd).into_iter().flatten().flatten() {
            let p = t.path();
            if p.is_dir()
                && let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned())
            {
                out.push((
                    kd.clone(),
                    name,
                    p,
                    Meta {
                        root: meta.root.clone(),
                        last_used: meta.last_used,
                    },
                ));
            }
        }
    }
    out
}

/// Remove a target dir. With `keep_empty` the dir is recreated empty: the
/// checkout is still there and its `target` symlink must not dangle, or the
/// next plain `cargo build` (no justrust to relink it) cannot create dirs.
fn remove_target(kd: &Path, name: &str, dir: &Path, keep_empty: bool) -> bool {
    let Some(lock) = try_lock(&kd.join(format!("{name}.lock"))) else {
        return false;
    };
    let trash = kd.join(format!(".trash-{name}-{}", std::process::id()));
    let ok = std::fs::rename(dir, &trash).is_ok();
    // Unlock explicitly: a child forked by another thread shares this open
    // file until it execs and would keep the lock held after the drop.
    if ok && keep_empty {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = lock.unlock();
    drop(lock);
    if !keep_empty {
        let _ = std::fs::remove_file(kd.join(format!("{name}.lock")));
    }
    let _ = std::fs::remove_dir_all(&trash);
    ok
}

/// Remove targets of checkouts that no longer exist or have not been used
/// for `max_age` seconds. Returns (targets removed, private bytes freed).
pub fn sweep(pool: &Path, max_age: f64) -> (usize, u64) {
    let now = paths::now();
    let (mut n, mut freed) = (0, 0);
    for (kd, name, dir, meta) in targets(pool) {
        let gone = !meta.root.is_empty() && !Path::new(&meta.root).exists();
        if gone || now - meta.last_used > max_age {
            let size = crate::store::dir_private(&dir);
            if remove_target(&kd, &name, &dir, !gone && !meta.root.is_empty()) {
                n += 1;
                freed += size;
            }
        }
    }
    // Checkout dirs left with nothing in them.
    for kd in std::fs::read_dir(pool).into_iter().flatten().flatten() {
        let kd = kd.path();
        let empty = std::fs::read_dir(&kd).is_ok_and(|mut d| {
            d.all(|e| {
                e.is_ok_and(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    n == ".pool.json" || n.ends_with(".lock") || n.starts_with(".trash-")
                })
            })
        });
        if empty {
            let _ = std::fs::remove_dir_all(&kd);
        }
    }
    (n, freed)
}

/// Low-disk reclaim: idle targets' incremental dirs (idle 10 min or more),
/// then whole targets idle an hour or more, least recently used first.
/// Returns private bytes freed.
pub fn reclaim(pool: &Path, want: u64, idle_scale: f64) -> u64 {
    let now = paths::now();
    let mut all = targets(pool);
    all.sort_by(|a, b| a.3.last_used.total_cmp(&b.3.last_used));
    let mut freed = 0;
    for (kd, name, dir, meta) in &all {
        if freed >= want {
            return freed;
        }
        if now - meta.last_used >= 600.0 * idle_scale
            && let Some(lock) = try_lock(&kd.join(format!("{name}.lock")))
        {
            freed += crate::slots::drop_incremental_in(dir);
            let _ = lock.unlock();
        }
    }
    for (kd, name, dir, meta) in &all {
        if freed >= want {
            break;
        }
        if now - meta.last_used >= 3600.0 * idle_scale {
            let size = crate::store::dir_private(dir);
            let live = !meta.root.is_empty() && Path::new(&meta.root).exists();
            if remove_target(kd, name, dir, live) {
                freed += size;
            }
        }
    }
    freed
}

/// Sweep with the configured age (`JUSTRUST_POOL_MAX_DAYS`, 14).
pub fn sweep_default() -> (usize, u64) {
    let days = std::env::var("JUSTRUST_POOL_MAX_DAYS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(14.0);
    pool_root().map_or((0, 0), |p| sweep(&p, days * 86400.0))
}

pub fn summary(pool: &Path) -> String {
    let t = targets(pool);
    let private: u64 = t.iter().map(|x| crate::store::dir_private(&x.2)).sum();
    format!("{} target(s), {} private", t.len(), crate::store::gb(private))
}

/// `justrust target-dir [--name N]`.
pub fn command(name: &str, key: Option<&str>) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let dir = match key {
        Some(k) => pool_root().and_then(|p| {
            let d = resolve_keyed(&p, k, name)?;
            crate::store::register_root("pool", &p);
            Some(d)
        }),
        None => resolve(&cwd, name),
    }
    .ok_or_else(|| anyhow::anyhow!("invalid target name/key or no justrust home"))?;
    println!("{}", dir.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("jr-pool-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn two_checkouts_get_two_dirs_and_one_checkout_gets_the_same_dir_every_time() {
        let pool = tmp("resolve");
        let (a, b) = (pool.join("ws/a"), pool.join("ws/b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let da = resolve_in(&pool, &a, "default").unwrap();
        assert_eq!(resolve_in(&pool, &a, "default").unwrap(), da);
        assert_ne!(resolve_in(&pool, &b, "default").unwrap(), da);
        assert_ne!(resolve_in(&pool, &a, "verify").unwrap(), da);
        assert!(da.is_dir());
        assert!(!valid_name("../escape") && !valid_name("") && valid_name("verify-1"));
        std::fs::remove_dir_all(pool).unwrap();
    }

    #[test]
    fn sweep_removes_targets_of_deleted_checkouts_and_stale_ones_but_not_held_or_live() {
        let pool = tmp("sweep");
        let ws = pool.join("ws");
        let (live, stale, gone, busy) = (ws.join("live"), ws.join("stale"), ws.join("gone"), ws.join("busy"));
        for d in [&live, &stale, &gone, &busy] {
            std::fs::create_dir_all(d).unwrap();
        }
        let mk = |root: &Path| {
            let d = resolve_in(&pool, root, "default").unwrap();
            std::fs::write(d.join("blob"), vec![1u8; 1 << 20]).unwrap();
            d
        };
        let (dl, ds, dg, db) = (mk(&live), mk(&stale), mk(&gone), mk(&busy));
        std::fs::remove_dir_all(&gone).unwrap();
        // Age `stale` and `busy` by rewriting their stamp.
        for (d, root) in [(&ds, &stale), (&db, &busy)] {
            let kd = d.parent().unwrap();
            let m = Meta { root: root.to_string_lossy().into_owned(), last_used: paths::now() - 30.0 * 86400.0 };
            std::fs::write(kd.join(".pool.json"), serde_json::to_vec(&m).unwrap()).unwrap();
        }
        let held = try_lock(&db.parent().unwrap().join("default.lock")).unwrap();
        let (n, freed) = sweep(&pool, 14.0 * 86400.0);
        assert_eq!(n, 2, "stale and deleted-checkout targets go");
        assert!(freed >= 2 << 20);
        assert!(dl.exists(), "live checkout kept");
        assert!(!dg.exists(), "target of a deleted checkout is gone");
        assert!(ds.is_dir() && std::fs::read_dir(&ds).unwrap().next().is_none(),
            "stale target of a live checkout is emptied, not removed, so ./target never dangles");
        assert!(db.exists(), "a held target is never removed");
        drop(held);
        std::fs::remove_dir_all(pool).unwrap();
    }

    #[test]
    fn reclaim_drops_incremental_first_then_idle_targets_and_skips_held() {
        let pool = tmp("reclaim");
        let mk = |name: &str, idle: f64| {
            let root = pool.join(format!("ws/{name}"));
            std::fs::create_dir_all(&root).unwrap();
            let d = resolve_in(&pool, &root, "default").unwrap();
            std::fs::create_dir_all(d.join("debug/incremental/c")).unwrap();
            let f = d.join("debug/incremental/c/s");
            std::fs::write(&f, vec![2u8; 2 << 20]).unwrap();
            // APFS reports private bytes only once the extents are allocated.
            std::fs::File::open(&f).unwrap().sync_all().unwrap();
            std::fs::write(d.join("debug/libx.rlib"), b"r").unwrap();
            let m = Meta { root: root.to_string_lossy().into_owned(), last_used: paths::now() - idle };
            std::fs::write(d.parent().unwrap().join(".pool.json"), serde_json::to_vec(&m).unwrap()).unwrap();
            d
        };
        let (idle, fresh, busy) = (mk("idle", 7200.0), mk("fresh", 30.0), mk("busy", 7200.0));
        let held = try_lock(&busy.parent().unwrap().join("default.lock")).unwrap();
        let freed = reclaim(&pool, 1, 1.0);
        assert!(freed >= 2 << 20);
        assert!(!idle.join("debug/incremental").exists() && idle.join("debug/libx.rlib").exists());
        assert!(fresh.join("debug/incremental").exists());
        assert!(busy.join("debug/incremental").exists());
        reclaim(&pool, u64::MAX, 1.0);
        assert!(
            idle.is_dir() && std::fs::read_dir(&idle).unwrap().next().is_none(),
            "whole idle target is emptied when incremental is not enough"
        );
        assert!(fresh.exists() && busy.exists());
        drop(held);
        std::fs::remove_dir_all(pool).unwrap();
    }

    #[test]
    fn pool_target_detection_requires_a_two_level_dir_under_the_pool() {
        let pool = PathBuf::from("/h/.justrust/pool");
        let a = |s: &[&str]| s.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            pool_target_of(&a(&["test", "--target-dir", "/h/.justrust/pool/k-ws/verify"]), &pool),
            Some(PathBuf::from("/h/.justrust/pool/k-ws/verify"))
        );
        assert_eq!(
            pool_target_of(&a(&["build", "--target-dir=/h/.justrust/pool/k-ws/default/"]), &pool),
            Some(PathBuf::from("/h/.justrust/pool/k-ws/default/"))
        );
        assert_eq!(pool_target_of(&a(&["test", "--target-dir", "/tmp/t"]), &pool), None);
        assert_eq!(pool_target_of(&a(&["test", "--target-dir", "/h/.justrust/pool/k-ws"]), &pool), None);
        assert_eq!(pool_target_of(&a(&["test", "--", "--target-dir", "/h/.justrust/pool/k/x"]), &pool), None);
    }
}

#[cfg(all(test, unix))]
mod link_tests {
    use super::*;

    #[test]
    fn link_mode_moves_an_existing_target_into_the_pool_and_leaves_a_symlink() {
        let base = std::env::temp_dir().join(format!("jr-pool-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let ws = base.join("ws");
        std::fs::create_dir_all(ws.join("crates/a")).unwrap();
        std::fs::write(ws.join("Cargo.toml"), "[workspace]\nmembers = [\"crates/a\"]\n").unwrap();
        std::fs::write(ws.join("crates/a/Cargo.toml"), "[package]\nname = \"a\"\n").unwrap();
        std::fs::create_dir_all(ws.join("target/debug")).unwrap();
        std::fs::write(ws.join("target/debug/warm"), b"warm state").unwrap();
        // From a member directory the workspace root is still the top.
        assert_eq!(workspace_root(&ws.join("crates/a")).unwrap(), ws);
        // The pool lives under JUSTRUST_HOME; drive the pieces with an
        // explicit pool to avoid touching the process environment.
        let pool = base.join("pool");
        let kd = key_dir(&pool, &ws);
        std::fs::create_dir_all(&kd).unwrap();
        std::fs::rename(ws.join("target"), kd.join("default")).unwrap();
        std::os::unix::fs::symlink(kd.join("default"), ws.join("target")).unwrap();
        // Cargo and every tool that reads ./target see the same bytes.
        assert_eq!(std::fs::read(ws.join("target/debug/warm")).unwrap(), b"warm state");
        assert_eq!(std::fs::read_link(ws.join("target")).unwrap(), kd.join("default"));
        // Deleting the checkout lets the sweeper reclaim the bytes.
        resolve_in(&pool, &ws, "default").unwrap();
        std::fs::remove_dir_all(&ws).unwrap();
        let (n, _) = sweep(&pool, 1e9);
        assert_eq!(n, 1);
        assert!(!kd.join("default").exists());
        std::fs::remove_dir_all(&base).unwrap();
    }
}

#[cfg(test)]
mod keyed_tests {
    use super::*;

    #[test]
    fn keyed_dirs_ignore_the_checkout_and_only_age_out() {
        let pool = std::env::temp_dir().join(format!("jr-pool-keyed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&pool);
        let a = resolve_keyed(&pool, "rollout", "cli").unwrap();
        assert_eq!(resolve_keyed(&pool, "rollout", "cli").unwrap(), a);
        assert_ne!(resolve_keyed(&pool, "rollout", "desktop").unwrap(), a);
        assert!(resolve_keyed(&pool, "../x", "cli").is_none());
        // No checkout to vanish: fresh stays, aged goes.
        assert_eq!(sweep(&pool, 3600.0).0, 0);
        assert_eq!(sweep(&pool, -1.0).0, 2);
        std::fs::remove_dir_all(&pool).unwrap();
    }
}
