# Store lab results (2026-11-25)

Method (scripts in this directory):

- Isolated 22 GB sparse APFS volume, so `df` is this lab's own bytes and its
  size is a hard ENOSPC. Peak and final are `df` used space minus the volume
  just before the lanes started, sampled every 2 s.
- jcode at `6376a17d4`, one worktree per lane, same SHA. Each lane runs
  `justrust test -p jcode-tui -- --skip login` (the login tests wait on a fixed
  port the owner's live jcode holds). `JUSTRUST_REMOTE=off`, 4 jobs per lane,
  `nice 10`.
- Both variants start from a `cp -c` clone of one warmed dependency cache (one
  cold build, 174 s), so the caches are byte-identical at the start.
- before = installed justrust 0.1.58. after = this fork, `JUSTRUST_POOL=1`,
  `JUSTRUST_DISK_MIN_FREE=1G` (the volume is 22 GB, not 926 GB).
- The machine ran the owner's other build lanes the whole time (load average
  about 17), so wall times are noisy. Disk numbers are not.
- Deviation from the brief: 2 to 3 lanes, not 5, and no Desktop build. One cold
  lane is 5.3 GB of disk, so five is about 26 GB, over the 20 GB experiment cap.
  An attempted Desktop run was aborted when the host volume itself reached 100%
  (the lab image had grown to 22 GB on the host).

## E1. Edit-test churn: 2 lanes, one cold build then 4 edit+test cycles each

| | before (0.1.58) | after (fork) |
| --- | --- | --- |
| disk after the two cold builds | +10.60 GB | +10.54 GB |
| disk growth over 8 edit+test cycles | +3.37 GB (0.42 GB per cycle) | +0.76 GB (0.10 GB per cycle) |
| peak | +14.77 GB | +12.08 GB |
| final | +13.96 GB | +11.29 GB |
| failed steps | 0 | 0 |
| wall | 339 s | 508 s |
| dependency cache hit rate | 66.3% (1282 / 653 miss) | 74.7% (1927 / 653 miss) |

Cold builds cost the same: a worktree's private bytes (incremental and its own
crates) are inherent. What changed is the churn: link outputs are no longer kept
without bound by the local-unit cache. The 508 s is the price: with 7 GB free
the second build in each lane waited for its reservation, and after 30 s the
waiting build reclaimed its idle neighbours' incremental dirs (the next cycle of
lane 2 took 208 s instead of 66 s, rebuilding that state).

Re-run with the final binary (after the local-unit stamp fix, which makes the
cache publish many more units): peak +11.97 GB, final +11.31 GB, growth over the
8 cycles +0.82 GB, 0 failed steps, wall 676 s (the machine was about 1.6x slower
than during the first runs: the same warm-up took 288 s instead of 174 s).
The disk result held.

## E2. Squeeze: 3 lanes, one cold build each, 11.7 GB free

| | before (0.1.58) | after (fork) |
| --- | --- | --- |
| lanes finished | 0 of 3 | 3 of 3 |
| result | all three failed `No space left on device` at 194 s, volume at 100%; the harness's own `touch` failed too | lane 3 waited 150 s for room, ran after lanes 1 and 2 finished |
| peak | volume full | +10.69 GB |
| wall | n/a (died) | 317 s |

The queue lines the build printed:
`justrust: waiting for disk space on ...: 8.5 GB free, 9.0 GB wanted, 1 other build(s) hold reservations (queued 1s)`.

## Portable local-unit cache across worktrees (`JUSTRUST_LOCALCACHE_PORTABLE=1`)

The first pilot (0.1.58) hit 63 of 90 units in a second worktree and none of the
heavy crates. Two causes, found with `finish.jsonl` (now records `why`):

1. **A bug, fixed here.** `finish` refused 45 of 48 jcode units with "output
   rewritten after staging": cargo moves the mtime of a finished output back by
   up to a second (same inode, same size), and the stamp compared mtime by
   equality. The check now refuses only a later mtime, a different size or a
   different inode. Published units went from 3 of 48 to 52 of 52. This also
   affects the default (non-portable) mode: 0.1.58 published almost no local
   rmeta/rlib units, which is why its local-unit hit rate was near 0.
2. **A correct refusal.** In the `test` profile 18 of 285 units, the heavy ones
   (`jcode_base`, `jcode_app_core`, `jcode_tui`), are refused with "output embeds
   the workspace or target path" (a compile-time `CARGO_MANIFEST_DIR` under the
   `test-support` feature). They are not path-independent, so they never hit
   across worktrees.

Same SHA, fresh worktree, warm dependency cache, release binary, host
machine under load:

| command | portable | wall | local units hit |
| --- | --- | --- | --- |
| `check -p jcode-base`, worktree 2 | on | 12.7 s | 44 of 48 |
| `check -p jcode-base`, worktree 3 | off | 15.3 s | 0 of 48 |
| `test -p jcode-tui --no-run`, worktree 2 | on | 109 s | 72 of 89 |
| `test -p jcode-tui --no-run`, worktree 3 | off | 139 s | 0 of 89 |

Worktree 2 target private bytes 3.05 GB against 3.68 GB. Portable stays opt-in:
it remaps the workspace path in outputs (`/jr-ws/...` in panic locations and
debug info), which changes what an agent reads in a failure.
