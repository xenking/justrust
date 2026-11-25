#!/bin/bash
# Acceptance check: ENOSPC mid-build leaves no corrupt store, and the next build recovers.
S=$JCODE_SCRATCH_DIR/jr/v
B=~/.jcode/backup/justrust-store-cache
hdiutil create -size 600m -type SPARSE -fs APFS -volname jrsmall $S/small.sparseimage >/dev/null 2>&1
hdiutil attach $S/small.sparseimage -mountpoint /Volumes/jrsmall -nobrowse >/dev/null 2>&1
cp -c -R $S/proj /Volumes/jrsmall/proj 2>/dev/null || cp -R $S/proj /Volumes/jrsmall/proj
rm -rf /Volumes/jrsmall/proj/target
cd /Volumes/jrsmall/proj
export JUSTRUST_HOME=/Volumes/jrsmall/jh JUSTRUST_REMOTE=off JUSTRUST_NO_UPDATE=1 JUSTRUST_NO_TELEMETRY=1 DO_NOT_TRACK=1 JUSTRUST_LOCAL_REASON=check
unset CARGO_TARGET_DIR
# 600 MB volume: scale the floor and reservation down from the 926 GB defaults.
export JUSTRUST_DISK_MIN_FREE=20M JUSTRUST_DISK_RESERVE=60M
df -m /Volumes/jrsmall | awk 'NR==2{print "free at start: "$4" MB"}'
echo "== fill the volume to ~3 MB free"
mkfile 3m /Volumes/jrsmall/pad
i=0; while mkfile 40m /Volumes/jrsmall/ballast$i 2>/dev/null; do i=$((i+1)); done
while mkfile 1m /Volumes/jrsmall/fine$i 2>/dev/null; do i=$((i+1)); done
rm /Volumes/jrsmall/pad
df -m /Volumes/jrsmall | awk 'NR==2{print "free: "$4" MB"}'
echo "== build with the guard OFF (old behaviour): expect ENOSPC"
JUSTRUST_DISK_GUARD=0 $B build --offline > $S/enospc1.log 2>&1; echo rc=$?
grep -m2 -i "no space" $S/enospc1.log | cut -c1-140
echo "store after failure:"; ls $JUSTRUST_HOME/cache/depcache/o 2>/dev/null | head -5; ls -a $JUSTRUST_HOME/cache/depcache/o 2>/dev/null | grep -c "^\.[a-z0-9]*\.tmp"
echo "== free the ballast, rebuild with the same store"
rm -f /Volumes/jrsmall/ballast* /Volumes/jrsmall/fine*
$B build --offline > $S/enospc2.log 2>&1; echo rc=$?
./target/debug/v
$B cache 2>&1 | grep -E "^size"
echo "== rebuild from a clean target: must hit the store and match"
rm -rf target; $B build --offline > $S/enospc3.log 2>&1; echo rc=$?; ./target/debug/v
$B cache 2>&1 | grep -E "hit rate|^size"
echo "== guard ON on a nearly full volume: queues, then builds anyway after the wait budget"
mkfile $(( $(df -m /Volumes/jrsmall | awk 'NR==2{print $4}') - 300 ))m /Volumes/jrsmall/ballast2
rm -rf target; JUSTRUST_DISK_WAIT_MIN=0.2 JUSTRUST_DISK_MIN_FREE=500M $B build --offline 2>&1 | grep -E "justrust: (only|waiting|disk still)" | cut -c1-150; echo rc=${PIPESTATUS[0]}
rm /Volumes/jrsmall/ballast2
