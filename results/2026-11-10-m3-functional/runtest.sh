#!/bin/bash
# usage: runtest.sh NAME BIN ADDR STREAM_S [VAR=VAL ...]
# env: CUT_WHEN (regex on stripped log; cut proxy CUT_DELAY s after it matches), CUT_DELAY, POST (s after cut before stop)
#      if CUT_WHEN unset, stop after STREAM_S seconds of scroll.
NAME=$1; BIN=$2; ADDR=$3; STREAM=$4; shift 4
LOG=/tmp/odtest/$NAME.log
strip() { sed 's/\x1b\[[0-9;]*m//g' "$LOG"; }
: > "$LOG"
env "$@" "$BIN" run --to="$ADDR" --encoder=vaapi --workspace=9 >"$LOG" 2>&1 &
PID=$!
echo "$NAME: sender pid $PID started $(date +%T)"
for i in $(seq 1 120); do
  strip | grep -q 'encoder:' && break
  sleep 0.5
done
echo "$NAME: encoder line at $(date +%T)"
SCROLL_T=$(date +%s)
hyprctl eval 'hl.exec_cmd([[foot --app-id=odbench sh -c "while :; do seq 1 2000000; done"]], { workspace = "9 silent" })'
echo "$NAME: scroll started $(date +%T)"
if [ -n "${CUT_WHEN:-}" ]; then
  for i in $(seq 1 240); do strip | grep -qE "$CUT_WHEN" && break; sleep 0.5; done
  sleep "${CUT_DELAY:-15}"
  echo "$NAME: cutting proxy $(date +%T)"
  pkill -USR1 -f '^python3 -I /tmp/odtest/proxy.py'
  sleep "${POST:-20}"
else
  sleep "$STREAM"
fi
echo "$NAME: stopping $(date +%T)"
pkill -f '^foot --app-id=odbench'
kill -INT "$PID" 2>/dev/null
for i in $(seq 1 30); do kill -0 "$PID" 2>/dev/null || break; sleep 0.5; done
if kill -0 "$PID" 2>/dev/null; then echo "$NAME: still alive after 15s SIGINT, SIGKILL"; kill -KILL "$PID"; fi
wait "$PID"; echo "$NAME: exit status $?"
