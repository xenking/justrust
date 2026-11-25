#!/bin/bash
# Acceptance check: GC during a running build must not evict what the build restored.
D=$JCODE_SCRATCH_DIR/jr/v
B=~/.jcode/backup/justrust-store-cache
export JUSTRUST_HOME=$D/home JUSTRUST_REMOTE=off JUSTRUST_NO_UPDATE=1 JUSTRUST_NO_TELEMETRY=1 DO_NOT_TRACK=1 JUSTRUST_LOCAL_REASON=check
export JUSTRUST_DEPCACHE_GRACE=0 JUSTRUST_DISK_GUARD=1
unset CARGO_TARGET_DIR
cd $D/proj
echo "== 1. cold build populates the store"
$B build --offline >/dev/null 2>&1; echo rc=$?
$B cache 2>&1 | grep -E "^size"
echo "== 2. fresh target dir; HOLD build restores deps from the store, then sleeps in build.rs"
rm -rf target
( HOLD_SECS=25 $B build --offline >$D/hold.log 2>&1; echo "hold build rc=$?" >> $D/hold.log ) &
sleep 12
echo "leases while building:"; $B store 2>&1 | grep -A2 "^leases"
echo "== 3. collect with a 1-byte cap and zero grace DURING the build"
JUSTRUST_STORE_MAX=1 JUSTRUST_DEPCACHE_MAX=1 $B store --gc 2>&1 | grep -E "evicted"
echo "entries left (pinned by the live lease):"; ls $JUSTRUST_HOME/cache/depcache/o | wc -l
wait
tail -1 $D/hold.log
echo "== 4. after the build ended, same collection"
JUSTRUST_STORE_MAX=1 JUSTRUST_DEPCACHE_MAX=1 $B store --gc 2>&1 | grep -E "evicted"
echo "entries left:"; ls $JUSTRUST_HOME/cache/depcache/o | wc -l
