#!/bin/sh
# gexcc-exec's machine-wide slots: 6 gexcc programs started at once from
# the host (6 gexc fit a 256-task scope), each holding for 3 s, may not
# run more than GEXCC_SLOTS (here
# 2, in a private slot directory) at a time; with the limit off (0), more
# than 2 overlap, which shows the measurement sees overlaps. Run inside a
# resource scope:
#   systemd-run --user --scope -p TasksMax=256 nice -n 19 tests/gexcc/slots/run.sh
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=${WORK:-$(mktemp -d)}
command -v gexcc > /dev/null || { echo "gexcc not on PATH"; exit 2; }
gexcc -O2 -o "$WORK/hold" "$HERE/hold.c" || exit 1
# most intervals open at once: +1 at each start, -1 at each end
overlap() { awk '{print $1, 1; print $2, -1}' "$1" | sort -n -k1,1 -k2,2n | awk '{c += $2; if (c > m) m = c} END {print m}'; }
burst() {
  rm -f "$WORK/log.$1"
  i=0
  while [ $i -lt 6 ]; do GEXCC_SLOTS=$1 GEXCC_SLOT_DIR=$WORK/slots "$WORK/hold" "$WORK/log.$1" 3000 & i=$((i + 1)); done
  wait
}
burst 2; burst 0
on=$(overlap "$WORK/log.2"); off=$(overlap "$WORK/log.0")
runs=$(cat "$WORK/log.2" | wc -l)
if [ "$runs" -eq 6 ] && [ "$on" -le 2 ] && [ "$off" -gt 2 ]; then
    echo "ok   slots (6 runs: at most $on at once with 2 slots, $off without)"
else
    echo "FAIL slots: $runs runs, $on at once with 2 slots (want <= 2), $off without (want > 2)"; exit 1
fi
