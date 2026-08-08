#!/bin/sh
# One nbench kernel, repeated, in the browser runtime. Prints the median.
#
#   tests/real-programs/kernel.sh <KERNEL> <module.wasm> [runs] [seconds]
#
# The full `nbench.sh` table cannot resolve anything small. Two seconds a
# kernel on a shared machine gives about 30% spread -- QEMU running the same
# binary three times gave 8.82, 11.19 and 11.46 on ASSIGNMENT -- so a change
# worth 10% is invisible in it, and a change worth 10% is most changes.
#
# This runs one kernel at a time, for longer, several times over, and reports
# the median with the spread beside it so the reader can see whether the
# number deserves any confidence. Comparing two modules means running this
# twice and looking at whether the ranges overlap.
#
# KERNEL is nbench's own name with spaces removed: NUMERICSORT, STRINGSORT,
# BITFIELD, FPEMULATION, FOURIER, ASSIGNMENT, IDEA, HUFFMAN, NEURALNET,
# LUDECOMPOSITION.
set -eu

KERNEL="${1:?usage: kernel.sh <KERNEL> <module.wasm> [runs] [seconds]}"
WASM="${2:?usage: kernel.sh <KERNEL> <module.wasm> [runs] [seconds]}"
RUNS="${3:-5}"
SECONDS_PER="${4:-4}"

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
WORK="${GEX_NBENCH_DIR:-${TMPDIR:-/tmp}/gex-nbench}"
D="$WORK/nbench-byte-2.2.3"

[ -x "$D/nbench" ] || { echo "skip: no nbench at $D (run nbench.sh once)"; exit 0; }
[ -f "$WASM" ] || { echo "skip: no module at $WASM"; exit 0; }
command -v node >/dev/null 2>&1 || { echo "skip: no node"; exit 0; }

# nbench's config turns kernels off by name; leaving one on is what makes
# this a measurement of that kernel rather than of the whole suite.
cfg="$D/ONLY.$$"
{
    echo "MINSECONDS=$SECONDS_PER"
    echo "ALLSTATS=F"
    for k in NUMSORT STRINGSORT BITFIELD EMF FOUR ASSIGN IDEA HUFF NNET LU; do
        echo "DO$k=F"
    done
} >"$cfg"
# The config name nbench wants is the flag, not the kernel: its own switches
# are abbreviated and do not match the printed titles.
case "$KERNEL" in
    NUMERICSORT) flag=NUMSORT ;;
    STRINGSORT) flag=STRINGSORT ;;
    BITFIELD) flag=BITFIELD ;;
    FPEMULATION) flag=EMF ;;
    FOURIER) flag=FOUR ;;
    ASSIGNMENT) flag=ASSIGN ;;
    IDEA) flag=IDEA ;;
    HUFFMAN) flag=HUFF ;;
    NEURALNET) flag=NNET ;;
    LUDECOMPOSITION) flag=LU ;;
    *) echo "unknown kernel $KERNEL"; exit 1 ;;
esac
sed -i "s/^DO$flag=F$/DO$flag=T/" "$cfg"

rates=""
i=0
while [ "$i" -lt "$RUNS" ]; do
    i=$((i + 1))
    r=$(node "$ROOT/shims/browser/js/run-node.mjs" \
            --dir "$D=$D" --cwd "$D" --argv0 "$D/nbench" \
            "$WASM" "$D/nbench" "-c$(basename "$cfg")" 2>/dev/null |
        awk -F: '/^ +: *[0-9]/ { gsub(/ /, "", $2); if ($2 + 0 > 0) { print $2; exit } }')
    [ -n "$r" ] || { echo "run $i produced nothing"; continue; }
    rates="$rates $r"
done
rm -f "$cfg"

echo "$rates" | tr ' ' '\n' | grep -v '^$' | sort -g | awk -v k="$KERNEL" '
    { v[NR] = $1 }
    END {
        if (NR == 0) { print "no results"; exit 1 }
        med = (NR % 2) ? v[(NR + 1) / 2] : (v[NR / 2] + v[NR / 2 + 1]) / 2
        printf "%-16s median %-12g  min %-12g max %-12g  spread %.1f%%  (n=%d)\n",
            k, med, v[1], v[NR], 100 * (v[NR] - v[1]) / med, NR
    }'
