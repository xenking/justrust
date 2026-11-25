#!/bin/bash
export JUSTRUST_HOME=/Volumes/jrlab/jh-warm
export JUSTRUST_REMOTE=off JUSTRUST_NO_UPDATE=1 JUSTRUST_NO_TELEMETRY=1 DO_NOT_TRACK=1 JUSTRUST_LOCAL_REASON=measure CARGO_BUILD_JOBS=5
unset CARGO_TARGET_DIR JCODE_SESSION_ID
cd /Volumes/jrlab/w0/jcode
s=$(date +%s)
nice -n 10 "$1" test -p jcode-tui -- --skip login > /Volumes/jrlab/warm.log 2>&1
echo "WARM_DONE rc=$? secs=$(( $(date +%s)-s ))" >> /Volumes/jrlab/warm.log
