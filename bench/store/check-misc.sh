#!/bin/bash
# Acceptance: pool link mode with a real build, lock protection, no dangling ./target, worktree removal, run retention.
D=$JCODE_SCRATCH_DIR/jr/v
B=~/.jcode/backup/justrust-store-cache
export JUSTRUST_HOME=$D/home3 JUSTRUST_REMOTE=off JUSTRUST_NO_UPDATE=1 JUSTRUST_NO_TELEMETRY=1 DO_NOT_TRACK=1 JUSTRUST_LOCAL_REASON=check
unset CARGO_TARGET_DIR
P=$D/pool-proj; cp -R $D/proj $P; rm -rf $P/target; cd $P
echo "== A1 first build in link mode"
JUSTRUST_POOL=1 $B build --offline >/dev/null 2>&1; echo rc=$?
echo "target is a symlink: $(test -L target && readlink target)"
echo "== A2 build in flight (HOLD 25s): sweep + reclaim with zero thresholds must not touch it"
( JUSTRUST_POOL=1 HOLD_SECS=25 $B build --offline >$D/hold3.log 2>&1; echo "hold rc=$?" >>$D/hold3.log ) &
sleep 14
JUSTRUST_POOL_MAX_DAYS=0 $B store --retention 2>&1 | grep retention
$B store --reclaim 1000 2>&1 | grep reclaim
echo "files in target during build: $(find target/ -type f | wc -l)"
wait; tail -1 $D/hold3.log
echo "== A3 build ended; same sweep now empties the idle target but ./target must not dangle"
sleep 1; JUSTRUST_POOL_MAX_DAYS=0 $B store --retention 2>&1 | grep retention
echo "files in target after sweep: $(find target/ -type f | wc -l); is dir: $(test -d target && echo yes || echo NO)"
echo "== A4 plain stock cargo (no justrust) still builds into the emptied managed dir"
cargo build --offline >/dev/null 2>&1; echo stock cargo rc=$?; ./target/debug/v
echo "== A5 delete the checkout; sweep removes its pool dir"
$B store 2>&1 | grep "^pool"
cd $D; rm -rf $P
$B store --retention >/dev/null 2>&1
$B store 2>&1 | grep "^pool"; ls $JUSTRUST_HOME/pool | wc -l
echo "== B retention of run records through the public interface"
cd $D/proj
for i in 1 2 3; do $B build --offline >/dev/null 2>&1; done
n0=$(ls $JUSTRUST_HOME/runs | wc -l)
for r in $(ls $JUSTRUST_HOME/runs | head -3); do touch -t 202611010000 $JUSTRUST_HOME/runs/$r; done
JUSTRUST_RUNS_MAX_DAYS=14 $B store --retention 2>&1 | grep retention
echo "runs before $n0 after $(ls $JUSTRUST_HOME/runs | wc -l)"
$B runs 2>&1 | grep -v telemetry | head -4
$B cache 2>&1 | grep -E "hit rate|^size"
