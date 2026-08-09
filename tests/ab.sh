#!/bin/sh
# Run two commands alternately and report whether they differ.
#
# Written after two measurements in a row were thrown away. The machine this
# runs on has a VM on it that comes and goes, so wall-clock drifts by tens of
# percent over minutes -- which is fatal to the usual shape of a benchmark,
# "run all of A, then run all of B". That shape attributes the drift between
# the two halves to the change.
#
# Alternating A B A B cancels drift to first order: each pair is minutes
# apart at most, and the median of per-pair *ratios* is what gets reported
# rather than the ratio of medians. A pair where the machine stalled moves
# both sides and its ratio stays near 1.
#
# It also prints the spread of A against itself, which is the number that
# says whether any of this is readable. If A's own runs spread 20%, a 10%
# difference between A and B means nothing, and the tool says so rather than
# leaving it to be noticed.
#
#   usage: tests/ab.sh N -- cmd-A... -- cmd-B...
#   e.g.   tests/ab.sh 5 -- ./run 32.wasm -- ./run 64.wasm
set -eu

N="${1:?usage: ab.sh N -- cmd-A... -- cmd-B...}"
shift
[ "${1:-}" = "--" ] || { echo "expected -- before command A" >&2; exit 2; }
shift

A=""
while [ $# -gt 0 ] && [ "$1" != "--" ]; do
    A="$A $(printf '%s' "$1" | sed "s/'/'\\\\''/g; s/^/'/; s/\$/'/")"
    shift
done
[ "${1:-}" = "--" ] || { echo "expected -- before command B" >&2; exit 2; }
shift
B=""
while [ $# -gt 0 ]; do
    B="$B $(printf '%s' "$1" | sed "s/'/'\\\\''/g; s/^/'/; s/\$/'/")"
    shift
done

WORK="${TMPDIR:-/tmp}/gex-ab.$$"
mkdir -p "$WORK"
trap 'rm -rf "$WORK"' EXIT

# Nanoseconds around one run, output discarded but exit status kept: a command
# that fails halfway is fast and would otherwise read as an improvement.
timed() {
    _s=$(date +%s%N)
    if ! eval "$1" >"$WORK/out" 2>"$WORK/err"; then
        echo "FAILED" >&2
        tail -3 "$WORK/err" >&2
        exit 1
    fi
    _e=$(date +%s%N)
    echo $(( (_e - _s) / 1000000 ))
}

i=1
: > "$WORK/a"; : > "$WORK/b"
while [ "$i" -le "$N" ]; do
    ta=$(timed "$A")
    tb=$(timed "$B")
    echo "$ta" >> "$WORK/a"
    echo "$tb" >> "$WORK/b"
    printf 'pair %d: A %sms  B %sms  ratio %s\n' "$i" "$ta" "$tb" \
        "$(awk "BEGIN{printf \"%.3f\", $tb/$ta}")"
    i=$((i + 1))
done

awk '
    function median(v, n,   c) { c = n % 2; return c ? v[(n+1)/2] : (v[n/2] + v[n/2+1]) / 2 }
    { a[NR] = $1 }
    END {
        n = NR
        for (i = 1; i <= n; i++) for (j = i+1; j <= n; j++)
            if (a[j] < a[i]) { t = a[i]; a[i] = a[j]; a[j] = t }
        printf "%.0f %.0f %.0f\n", median(a, n), a[1], a[n]
    }
' "$WORK/a" > "$WORK/astat"
read -r amed amin amax < "$WORK/astat"

awk '
    function median(v, n,   c) { c = n % 2; return c ? v[(n+1)/2] : (v[n/2] + v[n/2+1]) / 2 }
    { a[NR] = $1 }
    END {
        n = NR
        for (i = 1; i <= n; i++) for (j = i+1; j <= n; j++)
            if (a[j] < a[i]) { t = a[i]; a[i] = a[j]; a[j] = t }
        printf "%.0f\n", median(a, n)
    }
' "$WORK/b" > "$WORK/bstat"
read -r bmed < "$WORK/bstat"

# The median of the per-pair ratios, which is the drift-resistant statistic.
paste "$WORK/a" "$WORK/b" | awk '
    function median(v, n,   c) { c = n % 2; return c ? v[(n+1)/2] : (v[n/2] + v[n/2+1]) / 2 }
    { r[NR] = $2 / $1 }
    END {
        n = NR
        for (i = 1; i <= n; i++) for (j = i+1; j <= n; j++)
            if (r[j] < r[i]) { t = r[i]; r[i] = r[j]; r[j] = t }
        printf "%.4f\n", median(r, n)
    }
' > "$WORK/rstat"
read -r ratio < "$WORK/rstat"

echo
printf 'A median %sms   (spread %s..%sms, %.0f%% of the median)\n' \
    "$amed" "$amin" "$amax" \
    "$(awk "BEGIN{print 100*($amax-$amin)/$amed}")"
printf 'B median %sms\n' "$bmed"
printf 'median per-pair ratio B/A: %s  (%s%%)\n' "$ratio" \
    "$(awk "BEGIN{printf \"%+.1f\", 100*($ratio-1)}")"

# The honest verdict. A change smaller than A's own spread is not a result,
# and saying so here is cheaper than remembering to ask every time.
awk -v spread="$(awk "BEGIN{print 100*($amax-$amin)/$amed}")" \
    -v change="$(awk "BEGIN{print 100*($ratio-1)}")" 'BEGIN {
    c = change < 0 ? -change : change
    if (c < spread)
        printf "NOT READABLE: the %.1f%% difference is inside A'\''s own %.1f%% spread.\n", c, spread
    else
        printf "readable: %.1f%% is outside A'\''s own %.1f%% spread.\n", c, spread
}'
