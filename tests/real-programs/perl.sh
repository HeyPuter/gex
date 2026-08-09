#!/bin/sh
# Perl under gex: the largest interpreter in the suite, both ways.
#
# Perl is here because it is the shape of program the rest of the suite does
# not cover. gzip and zstd are one hot loop over a buffer; nbench is four. A
# language interpreter is a dispatch loop over a switch with a hundred arms,
# reached through a function pointer, with the hot data behind two pointer
# hops -- which is precisely the code where the bounds guard survives V8's
# optimiser and where a return-address stack would pay. It is also 3.7 MB of
# it, dynamically linked, reached through the guest's own `ld.so`.
#
# The script it runs is chosen so that every line is a different part of the
# interpreter and every line prints something a wrong flag would change:
#
#   a 20,000-key hash, then a sort with a Perl comparator -- the comparator is
#   called from C through a function pointer 200,000-odd times, so this is the
#   indirect-branch case at scale
#   a regex over 5,000 words, which is Perl's own bytecode engine
#   List::Util, which is an XS module: a shared object opened with dlopen
#   after the process is already running, so it exercises the path where code
#   appears at an address nothing knew about at translation time
#   an integer checksum over a built-up string, which is plain scalar work
#
# Checked against the host's own perl rather than a recorded expectation: the
# numbers are pure computation with no floating point, so a correct
# translation has to agree digit for digit with any perl anywhere.
#
#   usage: tests/real-programs/perl.sh [rootfs]
set -eu

ROOTFS="${1:-/tmp/debroot}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
GEXC="$ROOT/target/release/gexc"
SHIM="$ROOT/target/wasm32-unknown-unknown/release/gex_browser.wasm"
PERL="$ROOTFS/usr/bin/perl"
WORK="${TMPDIR:-/tmp}/gex-perl"

if [ ! -x "$GEXC" ]; then
    echo "skip: $GEXC is not built"
    exit 0
fi
if [ ! -f "$PERL" ]; then
    echo "skip: no aarch64 perl at $PERL"
    exit 0
fi
if ! command -v perl >/dev/null 2>&1; then
    echo "skip: no host perl to check against"
    exit 0
fi

mkdir -p "$WORK"
cat > "$ROOTFS/gex-perl-test.pl" <<'SCRIPT'
use strict; use warnings;
use List::Util qw(sum max);
my %h; my @w;
for my $i (1..20000) { my $k = "key$i"; $h{$k} = $i * $i % 97; push @w, $k; }
my @sorted = sort { $h{$a} <=> $h{$b} || $a cmp $b } @w;
printf "n=%d first=%s last=%s\n", scalar @sorted, $sorted[0], $sorted[-1];
my $text = join(" ", map { "word$_" } 1..5000);
my $c = () = $text =~ /\bword\d*[13579]\b/g;
print "odd words: $c\n";
printf "sum=%d max=%d\n", sum(values %h), max(values %h);
my $s = ""; $s .= chr(65 + ($_ % 26)) for 1..10000;
print "len=", length($s), " md5ish=", unpack("%32C*", $s) % 65521, "\n";
SCRIPT

perl "$ROOTFS/gex-perl-test.pl" > "$WORK/expected.txt"
echo "reference:"
sed 's/^/  /' "$WORK/expected.txt"

fail=0

# -- the runtime translator, under the native host -------------------------
echo "gexc run:"
if "$GEXC" run --sysroot "$ROOTFS" "$PERL" /gex-perl-test.pl > "$WORK/run.txt" 2>"$WORK/run.err"; then
    if cmp -s "$WORK/expected.txt" "$WORK/run.txt"; then
        echo "  matches"
    else
        echo "  DIFFERS:"
        diff "$WORK/expected.txt" "$WORK/run.txt" | sed 's/^/    /'
        fail=1
    fi
else
    echo "  FAILED (exit $?)"
    tail -3 "$WORK/run.err" | sed 's/^/    /'
    fail=1
fi

# -- static recompilation, which is the point ------------------------------
#
# The whole program and every library it needs, translated before it runs,
# then executed by the browser runtime under node. 84,000 blocks of it.
if ! command -v node >/dev/null 2>&1 || [ ! -f "$SHIM" ]; then
    echo "gexc build: skip, the browser shim is not built"
    [ "$fail" -eq 0 ] || exit 1
    exit 0
fi
echo "gexc build:"
"$GEXC" build "$PERL" -o "$WORK/perl.wasm" --no-shared-memory 2>&1 |
    grep -E "blocks|guards" | sed 's/^/  /'
if node "$ROOT/shims/browser/js/run-node.mjs" --dir "/=$ROOTFS" --cwd / \
        --argv0 perl "$WORK/perl.wasm" "$PERL" /gex-perl-test.pl \
        > "$WORK/build.txt" 2>"$WORK/build.err"; then
    if cmp -s "$WORK/expected.txt" "$WORK/build.txt"; then
        echo "  matches"
    else
        echo "  DIFFERS:"
        diff "$WORK/expected.txt" "$WORK/build.txt" | sed 's/^/    /'
        fail=1
    fi
else
    echo "  FAILED (exit $?)"
    tail -3 "$WORK/build.err" | sed 's/^/    /'
    fail=1
fi

rm -f "$ROOTFS/gex-perl-test.pl"
exit "$fail"
