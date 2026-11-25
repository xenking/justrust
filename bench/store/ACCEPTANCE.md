# Acceptance checks, mapped to the requirements

Each row was run against the release binary (`cargo build --release`) on real
cargo projects or real jcode builds, not only unit tests. Scripts: `check-pin.sh`,
`check-enospc.sh`, `check-misc.sh` in this directory (they use a scratch cargo
project with `itoa` and `fastrand`, and a `build.rs` that holds a build open).

| Requirement | Check | Observed |
| --- | --- | --- |
| Collection never deletes what a running build uses | `check-pin.sh`: real build holds open 25 s, `store --gc` with a 1-byte cap and zero grace during it | during the build: evicted 0, 2 entries kept. After it ended, the same call evicted both. `justrust store` listed the live lease (pid, 6 GB reserved) |
| No store corruption after ENOSPC | `check-enospc.sh`: 600 MB APFS volume filled to about 3 MB free, build with the guard off | `rc=101`, `No space left on device` writing a `.d` file. Store held 1 complete entry, 0 `.tmp` dirs. After freeing space the same store rebuilt (`rc=0`), the binary ran, and a clean-target rebuild succeeded, store consistent at 2 entries and 2 manifests |
| Low disk queues instead of dying | `check-enospc.sh` guard on; lab E2 (`RESULTS.md`) | 3 lanes on 11.7 GB free: 0 of 3 finished before, 3 of 3 after. On the small volume the build printed `waiting for disk space ... 0.3 GB free, 0.5 GB wanted`, then `building anyway` after the wait budget |
| No per-worktree `target` dirs | `check-misc.sh` A1 | `target` became a symlink into `<home>/pool/<key>/default` after a real `build`; an existing real `target/` is moved in by rename (also checked on a project that had one) |
| Held pool target never removed | `check-misc.sh` A2 | `JUSTRUST_POOL_MAX_DAYS=0` sweep and `store --reclaim 1000` during a build: 85 files still in `target/` |
| `./target` never dangles after a sweep | `check-misc.sh` A3, A4 | After the build ended the same sweep emptied the target (0 files), `target` was still a directory, and plain stock `cargo build` (no justrust) succeeded and ran. This check found a real bug (the sweep removed the dir and left a dangling symlink) that the unit tests missed; fixed |
| Target removed with the worktree | `check-misc.sh` A5 | after deleting the checkout, `justrust store` pool went from 1 target to 0 and the pool dir was gone |
| Retention for runs and records | `check-misc.sh` B | 3 of 5 run dirs older than 14 days removed, `justrust runs` and `cache` still work |
| jcode scripts use the managed dir, with fallback | python import of `accept-smoke.py` `pool_target` and `verify-jcode-desktop.py` `managed_build_dir` with the new and the old justrust on PATH | new: `<home>/pool/key-rollout-acceptance/cli`, stable across calls, and `<home>/pool/<key>-jcode-desktop/verify`. Old justrust: the legacy `<receipt>/target/cli` and `<repo>/target/verify-build` |
| Admission queues (jcode side) | `scripts/test_build_admission_reclaim.py`, new queue test | 4 of 4 pass; a build blocked by a full disk is admitted once room appears and prints `queued for disk space` |
| Slots GC by private bytes, incremental first, `--clean` keeps warm slots | unit tests in `slots.rs`; `justrust slots --clean --idle-min` | covered by tests only. Not exercised against live slots (agent mode needs a slot-eligible session) |
| Savings from the work | `RESULTS.md` E1 and E2 | churn +0.76 vs +3.37 GB over 8 cycles; ENOSPC 0 of 3 vs 3 of 3 |

Not covered by any real run: 5 concurrent lanes, a Desktop build, the live
daemon, `selfdev build` through `dev_cargo.sh` with the justrust proxy installed.
