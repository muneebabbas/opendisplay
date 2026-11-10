#!/bin/bash
# usage: run.sh shm|gpu  -> prints fps, encode, media MHz, cores
mode=$1; log=/tmp/m5ab/$mode-$(date +%s).log
F=/sys/class/drm/card0/device/tile0/gt1/freq0/act_freq
/tmp/od-receiver --listen 127.0.0.1:9100 --width 2388 --height 1668 --scale 2 --refresh 120 --sink none >/dev/null 2>&1 &
R=$!; sleep 2
if [ $mode = shm ]; then export OD_VAAPI_SHM_CAPTURE=1; else unset OD_VAAPI_SHM_CAPTURE; fi
/tmp/od-sender-m4 run --to=127.0.0.1:9100 --encoder=vaapi --workspace=9 >$log 2>&1 &
S=$!; sleep 6
hyprctl eval 'hl.exec_cmd([[mpv --no-config --title=odbench --fs --loop=inf --no-audio --really-quiet av://lavfi:testsrc2=size=1194x834:rate=120]], { workspace = "9 silent" })' >/dev/null
sleep 8
a=$(awk '{print $14+$15}' /proc/$S/stat); fs=()
for i in $(seq 15); do fs+=($(cat $F)); sleep 1; done
b=$(awk '{print $14+$15}' /proc/$S/stat)
pkill -f '^mpv .*odbench'; kill -INT $S; wait $S; kill $R; wait $R 2>/dev/null
med=$(printf '%s\n' "${fs[@]}" | sort -n | sed -n 8p)
python3 -I -c "print('$mode cores %.3f'%(($b-$a)/100/15), 'media MHz med $med')"
sed 's/\x1b\[[0-9;]*m//g' $log | grep "sent " | tail -3 | sed 's/ | SenderCounters.*//; s/.*INFO //'
