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

## E2. Squeeze: 3 lanes, one cold build each, 11.7 GB free

| | before (0.1.58) | after (fork) |
| --- | --- | --- |
| lanes finished | 0 of 3 | 3 of 3 |
| result | all three failed `No space left on device` at 194 s, volume at 100%; the harness's own `touch` failed too | lane 3 waited 150 s for room, ran after lanes 1 and 2 finished |
| peak | volume full | +10.69 GB |
| wall | n/a (died) | 317 s |

The queue lines the build printed:
`justrust: waiting for disk space on ...: 8.5 GB free, 9.0 GB wanted, 1 other build(s) hold reservations (queued 1s)`.

## Portable local-unit cache across worktrees (opt-in, pilot)

`JUSTRUST_LOCALCACHE_PORTABLE=1`, second worktree at the same SHA: 63 of 90 local
units hit, but none of `jcode_base`, `jcode_app_core`, `jcode_tui` (about 70% of
local compile time). Kept opt-in. Why those three miss is not established here.
