#!/bin/bash
# lab-lane.sh <variant> <bin> <lane> <cycles> <extra-env-file>
# One agent lane: initial `test -p jcode-tui`, then <cycles> x (edit, test).
V=$1; BIN=$2; L=$3; C=$4; ENVF=$5
LAB=/Volumes/jrlab
export JUSTRUST_HOME=$LAB/jh-$V
export JUSTRUST_REMOTE=off JUSTRUST_NO_UPDATE=1 JUSTRUST_NO_TELEMETRY=1 DO_NOT_TRACK=1
export JUSTRUST_LOCAL_REASON=measure CARGO_BUILD_JOBS=4
unset CARGO_TARGET_DIR JCODE_SESSION_ID
[ -f "$ENVF" ] && source "$ENVF"
export JUSTRUST_SLOT_SESSION=lane-$L
cd $LAB/$V/w$L/jcode || exit 9
OUT=$LAB/$V/lane$L.log
step() { # name args...
  local n=$1; shift
  local s=$(date +%s)
  nice -n 10 "$BIN" "$@" -- --skip login >> $OUT 2>&1
  local rc=$?
  echo "STEP lane=$L step=$n rc=$rc secs=$(( $(date +%s)-s )) t=$(date +%s)" >> $LAB/$V/steps.log
}
step initial test -p jcode-tui
for i in $(seq 1 $C); do
  echo "// bench lane $L cycle $i" >> crates/jcode-tui/src/lib.rs
  step cycle$i test -p jcode-tui
done
echo "LANE_DONE $L" >> $LAB/$V/steps.log
