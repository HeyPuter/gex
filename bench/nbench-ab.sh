#!/bin/sh
# A/B two builds of gexc on nbench, kernel by kernel, in the browser runtime.
#
#   usage: bench/nbench-ab.sh <gexc-A> <gexc-B> [work-dir]
#
# Ten kernels with very different shapes. `bench/ab-bin.sh` reports one number
# per program, which hides a change that helps integer code and hurts floating
# point; this reports ten, so it cannot.
#
# The metric is nbench's own iterations per second, which it calibrates
# against the clock -- so unlike the CPU-time harness this *is* wall-clock
# sensitive, and a loaded machine moves it. MINSECONDS is set high enough that
# each kernel settles, and each side is run `--reps` times with the best kept,
# which is the shape least disturbed by something else on the box.
set -eu

A="${1:?gexc for side A}"
B="${2:?gexc for side B}"
WORK="${3:-${TMPDIR:-/tmp}/gex-nbench}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
D="$WORK/nbench-byte-2.2.3"

# One run at a time per work directory. Both sides build into `$WORK/a.wasm`
# and `$WORK/b.wasm` and append to `$WORK/a.all`, so two runs sharing a
# directory silently read each other's numbers -- which looks like a result
# rather than like a crash. Pass a third argument to run two at once.
mkdir -p "$WORK"
if ! mkdir "$WORK/.lock" 2>/dev/null; then
    echo "skip: $WORK is in use by another run (remove $WORK/.lock if not)"
    exit 0
fi
trap 'rmdir "$WORK/.lock" 2>/dev/null' EXIT INT TERM
REPS="${GEX_BENCH_REPS:-3}"

if [ ! -x "$D/nbench" ]; then
    echo "skip: $D/nbench is not built -- see tests/real-programs/nbench.sh"
    exit 0
fi
printf 'MINSECONDS=%s\nALLSTATS=F\n' "${GEX_NBENCH_SECONDS:-3}" >"$D/NBCFG"

extract() {
    awk -F: '
        /^[A-Z][A-Z ]+ +:/ {
            gsub(/ +$/, "", $1); name = $1; v = $2; gsub(/ /, "", v)
            if (v != "") { print name "\t" v; name = "" }
            next
        }
        /^ +: *[0-9]/ && name != "" { v = $2; gsub(/ /, "", v); print name "\t" v; name = "" }
    ' | grep -Ev 'INDEX|^TEST'
}

# The best of `REPS` runs, per kernel: a kernel that was interrupted reads low
# and never high, so the maximum is the estimate least polluted by the rest of
# the machine.
best() {
    side="$1"; gexc="$2"
    "$gexc" build --exhaustive --no-shared-memory "$D/nbench" -o "$WORK/$side.wasm" >/dev/null 2>&1
    i=0
    : >"$WORK/$side.all"
    while [ "$i" -lt "$REPS" ]; do
        node "$ROOT/shims/browser/js/run-node.mjs" \
            --dir "$D=$D" --cwd "$D" --argv0 "$D/nbench" \
            "$WORK/$side.wasm" "$D/nbench" -cNBCFG 2>/dev/null | extract >>"$WORK/$side.all"
        i=$((i + 1))
    done
    sort "$WORK/$side.all" | awk -F'\t' '
        { if (!($1 in m) || $2 + 0 > m[$1]) m[$1] = $2 + 0 }
        END { for (k in m) printf "%s\t%.6g\n", k, m[k] }
    ' | sort >"$WORK/$side.txt"
}

best a "$A"
best b "$B"

printf '%-20s %-13s %-13s %s\n' KERNEL A B B/A
join -t"$(printf '\t')" "$WORK/a.txt" "$WORK/b.txt" | awk -F'\t' '
    { r = $3 / $2; printf "%-20s %-13s %-13s %5.3f\n", $1, $2, $3, r; s += log(r); n++ }
    END { if (n) printf "%-20s %-13s %-13s %5.3f\n", "(geomean)", "", "", exp(s / n) }
'
