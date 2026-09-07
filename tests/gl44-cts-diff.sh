#!/usr/bin/env bash
# Which cases pass natively and fail in the browser, for one CTS filter.
#
# A browser-only failure is the most valuable kind of conformance defect here:
# the native driver already proves the GL semantics are right, so the gap is
# in the transport or in what the browser's WebGPU accepts, and the browser is
# the target. A native-only failure is a driver bug; a shared failure is
# usually a platform wall. This script separates the three.
#
#   tests/gl44-cts-diff.sh 'KHR-GL44.buffer_storage.*'
#
# GEX_DIFF_KEEP=1 leaves the two logs in place for reading.
set -uo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
CASE=${1:?usage: gl44-cts-diff.sh <deqp case filter>}
BROWSER_LOG=$(mktemp /tmp/gex-cts-diff-browser.XXXXXX)
NATIVE_LOG=$(mktemp /tmp/gex-cts-diff-native.XXXXXX)

# Per-case verdicts as "name<TAB>result", from either runner's qpa stream.
verdicts() {
    awk '
        /QPA-CASE/   { sub(/.*QPA-CASE  */, ""); sub(/[ \t]+$/, ""); name = $0 }
        /QPA-RESULT/ { sub(/.*QPA-RESULT  */, ""); split($0, f, " ");
                       if (name != "") { print name "\t" f[1]; name = "" } }
    ' "$1" | sort -u
}

# The two runners take their case filter differently, and naming the wrong one
# is a quiet way to measure nothing: gl-cts-native.sh reads --deqp-case from its
# arguments, gl44-cts-browser.sh reads GEX_CTS_CASE from the environment. Each
# ignores the other's spelling and runs its default single case instead, which
# reports as a clean 1/1 pass.
echo "== native =="
"$ROOT/tests/gl-cts-native.sh" --deqp-case="$CASE" > "$NATIVE_LOG" 2>&1
grep -E 'Passed:|Failed:' "$NATIVE_LOG" | tail -2

echo "== browser =="
GEX_CTS_CASE="$CASE" GEX_CTS_IDLE=${GEX_CTS_IDLE:-420} GEX_CTS_WALL=${GEX_CTS_WALL:-1500} \
    "$ROOT/tests/gl44-cts-browser.sh" > "$BROWSER_LOG" 2>&1
grep -E 'Passed:|Failed:' "$BROWSER_LOG" | tail -2

verdicts "$NATIVE_LOG" > /tmp/gex-cts-diff-n.tsv
verdicts "$BROWSER_LOG" > /tmp/gex-cts-diff-b.tsv

# Absence of failures is not evidence. A run that executed one case, or none,
# looks identical to a family that passed -- so say the count out loud and
# refuse when either side is empty.
n_cases=$(grep -c 'QPA-CASE' "$NATIVE_LOG" 2>/dev/null || echo 0)
b_cases=$(grep -c 'QPA-CASE' "$BROWSER_LOG" 2>/dev/null || echo 0)
echo "cases executed: native=$n_cases browser=$b_cases"
if [ "$n_cases" -eq 0 ] || [ "$b_cases" -eq 0 ]; then
    echo "gl44-cts-diff: a side executed no cases -- nothing was measured." >&2
    echo "  logs: $NATIVE_LOG $BROWSER_LOG" >&2
    exit 3
fi

# A case absent from one side ran on neither, or the run stopped early; say so
# rather than counting it as agreement.
join -t $'\t' -a1 -a2 -e MISSING -o 0,1.2,2.2 /tmp/gex-cts-diff-n.tsv /tmp/gex-cts-diff-b.tsv |
awk -F'\t' '
    $2 == "Pass" && $3 != "Pass" { bo = bo sprintf("  %-64s native=%s browser=%s\n", $1, $2, $3); nb++ }
    $2 != "Pass" && $3 == "Pass" { no = no sprintf("  %-64s native=%s browser=%s\n", $1, $2, $3); nn++ }
    $2 != "Pass" && $3 != "Pass" { sh++ }
    END {
        printf "\nbrowser-only failures (transport or WebGPU): %d\n", nb+0; printf "%s", bo
        printf "\nnative-only failures (driver): %d\n", nn+0; printf "%s", no
        printf "\nfailing on both (usually a platform wall): %d\n", sh+0
    }'
[ -n "${GEX_DIFF_KEEP:-}" ] && echo "logs: $NATIVE_LOG $BROWSER_LOG" || rm -f "$NATIVE_LOG" "$BROWSER_LOG"
exit 0
