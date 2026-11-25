# One disk governor for every build cache

Goal: N agents building the same repo in N worktrees do not cost N times the
disk, a fresh worktree builds warm, disk use stays under a cap, collection never
breaks a running build, and a nearly full volume queues builds instead of
killing them with ENOSPC.

## What was measured (macOS, APFS, 926 GB volume, five agent lanes)

| Observation | Number |
| --- | --- |
| `du` of one build slot vs bytes deleting it frees | 40.1 GB vs 6.3 GB private |
| `du` of the shared `target/debug` vs private | 50.3 GB vs 17.8 GB private |
| local-unit cache (`localcache`) vs its 10 GB cap | 23 GB, 47 link outputs = 14.7 GB of it |
| one cold `test -p jcode-tui` in a fresh worktree | 7.0 GB of disk, 3.2 GB of it `incremental/` |
| portable local-unit hits across worktrees | 63 of 90 units, 0 of the 4 crates that hold ~70% of compile time |

Method: `ATTR_CMNEXT_PRIVATESIZE` (bytes a file frees if deleted) summed per
tree, `df` on an isolated sparse APFS volume for peak/final.

## Why the disk filled

1. **`du` and `st_blocks` count every clone in full.** A slot is a `cp -c` clone
   of the shared target, so a budget in `du` bytes (`JUSTRUST_SLOTS_MAX_GB`) is
   measured in bytes that do not exist, deletes cheap slots, and never sees the
   files that fill the volume.
2. **Link outputs are published to the local-unit cache on every miss.** A test
   binary is 300-640 MB and hits only on a revert. The cache keeps a clone, so
   the next build overwriting its target file leaves the cache as the only
   owner: one binary of disk per build per agent. The cap was checked every ten
   minutes and entries were immune for ten more.
3. **Incremental state is the private part of a worktree** (3.2 of 4.3 GB) and
   nothing owned it: it lived in `<worktree>/target`, invisible to every
   budget, and outlived the worktree.
4. **Nothing looked at free space.** Builds started, wrote, and died.
5. `justrust slots --clean` removed warm slots between an agent's commands.

## Design

Sharing happens only through the content-addressed store, never through a shared
target dir: Cargo does not put the checkout path in the hash of workspace units,
so two worktrees in one target overwrite each other (FINDINGS 2026-10-10).

- **Store** (existing `depcache` for registry/git units, `localcache` for
  workspace units): content-addressed, reflink restore, atomic publish
  (`.tmp` dir then rename, so a killed or ENOSPC build leaves an invisible
  dot-dir that collection removes). New: one cap and one LRU across both
  (`JUSTRUST_STORE_MAX`, default a quarter of free disk, 30-150 GB).
- **Link outputs are a separate class.** Executables of 8 MiB or more expire
  after 30 idle minutes (`JUSTRUST_EPHEMERAL_IDLE_MIN`) and are capped
  (`JUSTRUST_EPHEMERAL_MAX`, a quarter of the cap, 2-8 GB), 2 minute grace,
  collected every two minutes. Only the last binary of a revert is worth it.
- **Private state is accounted by private bytes** (`store::private_bytes`).
  Slot GC drops `incremental/` of idle slots first, then whole slots.
- **Pool targets** (`justrust target-dir`, `JUSTRUST_POOL=1`): `<workspace>/target`
  becomes a symlink into `~/.justrust/pool/<key>/default`. Every tool that
  expects `./target/...` is unchanged. The bytes are registered, reclaimed under
  pressure (incremental first), removed when the worktree is deleted, and aged
  out after 14 days. A held lock (a running build) is never touched.
  `--key K` gives builds that run in a fresh checkout path each time (rollout
  acceptance) a stable dir.
- **Leases** (`store::admit`): every local build takes a flock lease that
  reserves headroom (`check` 2 GB, `test` 4 GB, `build` 6 GB) against free
  space less a floor (`JUSTRUST_DISK_MIN_FREE`, 2% of the volume, 6-24 GB). N
  builds starting together cannot each see the same free space. A build that
  does not fit reclaims once, then queues, printing why, up to
  `JUSTRUST_DISK_WAIT_MIN` (45), then runs anyway: never worse than before. A
  killed build's lease disappears with its process.
- **Pins**: routine collection skips store entries used since the oldest live
  lease began, so a running build never loses what it restored. Leases older
  than six hours stop pinning.
- **Low-disk reclaim** (`store::reclaim`, also `justrust store --reclaim GB`):
  idle link outputs, idle slots' and pool targets' incremental dirs, idle slots
  and pool targets, then LRU entries whose private bytes are at least 1 MiB
  (an entry that is a clone of a live file frees nothing).
- **Retention**: run records 1500 / 14 days / 1 GB, run index, journals and
  logs bounded to their newest bytes, hint snapshots 30 days.
- `append_line` starts a new line after a torn last line (ENOSPC), so one torn
  write no longer loses the next one.

## Prior art and why not

- **sccache**: its key includes absolute target-dir paths; 0 of 86 hits (FINDINGS).
  No incremental.
- **Shared `target-dir` + Cargo `build-dir` split**: unit hashes omit the
  checkout, so worktrees collide, and legacy locking serializes agents.
- **Hardlinks**: forbidden for mutable fingerprints and incremental files.
- **Reflink store with dedupe** (this design): zero extra bytes for hits, and on
  APFS free accounting needs private size, not `du`.

## Not done, and why

- **Cross-worktree reuse of jcode's heavy crates.** `jcode_base`, `jcode_app_core`
  and `jcode_tui` embed the workspace path (`CARGO_MANIFEST_DIR`), so the portable
  guard correctly refuses to publish them. Portable mode stays opt-in
  (`JUSTRUST_LOCALCACHE_PORTABLE=1`): measured 63 of 90 unit hits, none of the
  expensive ones. A path-independent build of those crates is the lever.
- **Single-flight of identical in-flight units** across simultaneous cold lanes.

## Commands

```
justrust store [--gc] [--reclaim GB] [--retention]   volume, leases, store and pool sizes
justrust target-dir [--name N] [--key K]            managed target dir for this checkout
justrust slots --clean [--idle-min 30]               keeps recently used slots
JUSTRUST_POOL=1                                      make ./target a managed symlink
JUSTRUST_DISK_GUARD=0                                turn admission and reclaim off
```

Measurement lab: `bench/store/` (isolated sparse APFS volume, same SHA, before
and after binaries, `collect.py` for peak and final disk, wall time, hit rates).
