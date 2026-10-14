#!/bin/bash
# The guest-side legs of the demo (docs/gexcc.md §9), in the page, through
# tests/demo/desktop.mjs: /proc lists every process (ps, htop's source), a
# pty in raw mode reads keys, nvim takes typed keys, an epoll set survives
# fork, ttyname and ps's TTY column, and 40 MiB through a pipeline (dd, cat
# and wc: two pipes between three processes) in under a second. Run on edger:
#
#   scratch/bench/bench.sh -s $PWD/web/dist-demo -s $PWD/tests -- \
#       bash tests/demo/guest.sh [DIST]
set -eu
cd "$(dirname "$0")/../.."
DIST=${1:-web/dist-demo}
b64() { base64 -w0 "tests/demo/guest/$1"; }
STEPS=$(cat <<JSON
[
{"install":"python"},{"install":"nvim"},{"install":"htop"},
{"run":"n=\$(ls /proc | grep -c '^[0-9][0-9]*\$'); echo \$n pids; ps ax | head -5; test \$n -ge 3 && ps ax | grep -q '[x]term'","label":"procfs lists every process","show":true,"wall":60},
{"run":"echo $(b64 tty.py) | base64 -d > /tmp/guest-tty.py; python3.13 /tmp/guest-tty.py","label":"terminals and fork","show":true,"wall":120},
{"run":"echo $(b64 nvim.py) | base64 -d > /tmp/guest-nvim.py; python3.13 /tmp/guest-nvim.py","label":"nvim on a pty","show":true,"wall":120},
{"run":"s=\$EPOCHREALTIME; n=\$(dd if=/dev/zero bs=64k count=640 2>/dev/null | cat | wc -c); e=\$EPOCHREALTIME; ms=\$(( (\${e/./}-\${s/./})/1000 )); echo \"\$n bytes in \$ms ms, \$(( n / 1000 / (ms + 1) )) MB/s\"; test \$n = 41943040 && test \$ms -lt 1000","label":"40 MiB through cat","show":true,"wall":120}
]
JSON
)
DEMO_LEGS=${DEMO_LEGS:-0} DEMO_WALL=${DEMO_WALL:-600} DEMO_TAIL=${DEMO_TAIL:-20} DEMO_STEPS="$STEPS" \
    taskset -c "${BENCH_CORES:-2,4,6,8}" node tests/demo/desktop.mjs "$DIST"
