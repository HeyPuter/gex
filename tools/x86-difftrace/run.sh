#!/bin/sh
# Find the first instruction gex runs differently from the host CPU.
#
#   tools/x86-difftrace/run.sh LO HI GEXC PROG [ARGS...]
#
# Runs a static i386 PROG natively, single-stepped under ptrace
# (steptrace.c), and under `GEXC run` with GEX_TRACE_INSNS=LO-HI, each
# writing the registers and flags before every instruction in [LO, HI)
# (hex), and prints the first line where the two differ (cmp.py). The
# program should place what it traces at fixed addresses (code, data,
# stack) so the traces compare register for register. Native single-step
# runs at about 80k instructions a second.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
lo=$1 hi=$2 gexc=$3; shift 3
work=${WORK:-$(mktemp -d)}
mkdir -p "$work"
cc -O2 -o "$work/steptrace" "$here/steptrace.c"
"$work/steptrace" "$lo" "$hi" "$work/native.trace" "$@"
GEX_TRACE_INSNS="$lo-$hi" GEX_TRACE_FILE="$work/gex.trace" "$gexc" run --no-cache "$@" > /dev/null || true
python3 "$here/cmp.py" "$work/native.trace" "$work/gex.trace"
echo "traces in $work"
