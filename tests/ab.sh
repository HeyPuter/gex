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
# The verdict is a sign test over the pairs -- how often did B lose? -- which
# is exact under the null and assumes nothing about how run times are
# distributed. A's raw spread is printed too, but deliberately not used as the
# threshold: it contains all the drift that pairing exists to remove, so
# judging against it would call everything unreadable on a machine like this
# one. That was this script's first mistake.
#
#   usage: tests/ab.sh [--metric RE] N -- cmd-A... -- cmd-B...
#   e.g.   tests/ab.sh 5 -- ./run 32.wasm -- ./run 64.wasm
set -eu

# --metric RE reads the number matched by RE out of the command's stderr and
# uses that instead of wall-clock. The point is to measure the thing under
# test rather than the process around it: a statically recompiled perl is nine
# seconds of which two to four are V8 compiling the module, so a codegen change
# measured on wall-clock is a third diluted before it starts.
METRIC=""
if [ "${1:-}" = "--metric" ]; then
    METRIC="$2"
    shift 2
fi

N="${1:?usage: ab.sh [--metric RE] N -- cmd-A... -- cmd-B...}"
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
    if [ -n "$METRIC" ]; then
        # First number on the first matching line. A run that did not print it
        # is a broken invocation, not a fast one, and says so.
        _m=$(grep -m1 -E "$METRIC" "$WORK/err" 2>/dev/null |
             grep -oE '[0-9]+' | head -1)
        if [ -z "$_m" ]; then
            echo "no line matching '$METRIC' on stderr" >&2
            tail -3 "$WORK/err" >&2
            exit 1
        fi
        echo "$_m"
    else
        echo $(( (_e - _s) / 1000000 ))
    fi
}

# ABBA, not ABAB.
#
# Running A first in every pair is a bias, and a large one. Feeding this script
# the *same command* as both sides -- which should give a ratio of 1 -- gave
# B a 29% advantage over three pairs, consistently, because whatever the first
# run of a pair warms (the on-disk translation cache, the page cache holding a
# multi-megabyte module) the second run gets for free.
#
# So the order flips every pair: A B, then B A. Whatever second place is worth
# is then paid equally by both sides, and the identical-command test comes back
# to 1.00 as it should.
i=1
: > "$WORK/a"; : > "$WORK/b"
while [ "$i" -le "$N" ]; do
    if [ $((i % 2)) -eq 1 ]; then
        ta=$(timed "$A"); tb=$(timed "$B"); order="AB"
    else
        tb=$(timed "$B"); ta=$(timed "$A"); order="BA"
    fi
    echo "$ta" >> "$WORK/a"
    echo "$tb" >> "$WORK/b"
    printf 'pair %d (%s): A %sms  B %sms  ratio %s\n' "$i" "$order" "$ta" "$tb" \
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

# The honest verdict, by sign test on the pairs.
#
# Not by comparing the change to A's raw spread, which is what this did first
# and which is the wrong test for a paired design: A's spread includes all the
# drift that pairing exists to cancel, so it would call everything unreadable
# on a machine like this one. What pairing buys is that each pair is its own
# controlled experiment, and the question becomes "how often did B lose?" --
# which has an exact answer under the null with no assumption about the
# distribution of times.
#
# Two-sided binomial tail on the number of pairs favouring one side.
paste "$WORK/a" "$WORK/b" | awk '
    function choose(n, k,   i, r) {
        if (k < 0 || k > n) return 0
        r = 1
        for (i = 0; i < k; i++) r = r * (n - i) / (i + 1)
        return r
    }
    { n++; if ($2 > $1) wins++; else if ($2 < $1) losses++ }
    END {
        k = wins > losses ? wins : losses
        tail = 0
        for (i = k; i <= n; i++) tail += choose(n, i)
        p = 2 * tail / (2 ^ n)
        if (p > 1) p = 1
        printf "%d of %d pairs favour %s (p = %.3f by sign test)\n",
            k, n, (wins > losses ? "A" : "B"), p
        if (p <= 0.05)
            print "readable: the sides differ."
        else if (p <= 0.15)
            printf "SUGGESTIVE ONLY: p = %.3f. Run more pairs before believing it.\n", p
        else
            printf "NOT READABLE: p = %.3f, which is what a coin does.\n", p
    }
'
