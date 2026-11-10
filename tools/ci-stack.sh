#!/usr/bin/env bash
# Run upstream's CI checks on every branch of the stack (top first), via od-cargo.
#   ci-stack.sh <log-dir> <branch>...
set -uo pipefail
logdir=$1; shift
mkdir -p "$logdir"
cd ~/projects/opendisplay/linux
C=~/projects/opendisplay-fork/tools/od-cargo
for b in "$@"; do
  git -C .. checkout -q --detach "$b" || { echo "$b: checkout failed"; continue; }
  log="$logdir/${b//\//_}.log"
  ok=1
  for cmd in "fmt --all --check" "clippy --locked --workspace --all-targets -- -D warnings" \
             "test --locked --workspace" "test --locked -p od-sender --no-default-features"; do
    echo "### $cmd" >>"$log"
    $C $cmd >>"$log" 2>&1 || { ok=0; echo "FAILED: $cmd" >>"$log"; }
  done
  [ $ok = 1 ] && echo "$b: CI OK" || echo "$b: CI FAILED (see $log)"
done
