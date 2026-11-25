#!/bin/bash
# lab-run.sh <variant> <bin> <lanes> <cycles> <env-file> <sha>
V=$1; BIN=$2; N=$3; C=$4; ENVF=$5; SHA=${6:-6376a17d4}
LAB=/Volumes/jrlab
SRC=~/Projects/github.com/xenking/jcode
rm -rf $LAB/jh-$V $LAB/$V; mkdir -p $LAB/$V
cp -Rc $LAB/jh-warm $LAB/jh-$V
for l in $(seq 1 $N); do git -C $SRC worktree add -q --detach $LAB/$V/w$l/jcode $SHA; done
base=$(df -m $LAB | awk 'NR==2{print $3}')
echo "base_used_mb=$base" > $LAB/$V/meta.txt
rm -f $LAB/$V/df.log $LAB/$V/df.stop
( while [ ! -e $LAB/$V/df.stop ]; do echo "$(date +%s) $(df -m $LAB | awk 'NR==2{print $3}')" >> $LAB/$V/df.log; sleep 2; done ) &
SAMP=$!
t0=$(date +%s); echo "t0=$t0" >> $LAB/$V/meta.txt
pids=()
for l in $(seq 1 $N); do "$(dirname "$0")/lab-lane.sh" $V $BIN $l $C $ENVF & pids+=($!); done
wait "${pids[@]}"
t1=$(date +%s); echo "t1=$t1 wall=$((t1-t0))" >> $LAB/$V/meta.txt
sleep 5   # let detached finishers publish
echo "end_used_mb=$(df -m $LAB | awk 'NR==2{print $3}')" >> $LAB/$V/meta.txt
touch $LAB/$V/df.stop; wait $SAMP 2>/dev/null
echo DONE >> $LAB/$V/meta.txt
