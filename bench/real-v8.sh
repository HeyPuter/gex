#!/bin/sh
# A/B two `gexc` builds on real programs from a real distribution, on V8.
#
#   usage: bench/real-v8.sh <gexc-A> <gexc-B> <debian-rootfs> [work-dir]
#
# This is the harness the others were missing. `ab-bin.sh` and `suite.sh` run
# programs written for the occasion; `real.sh` runs zstd, which is real, but
# through the *native* adapter, because zstd starts a thread even at `-T1` and
# a guest thread needs a SharedArrayBuffer. `nbench-ab.sh` is ten kernels of
# straight-line arithmetic. None of them is a dynamically linked program out
# of a distribution, statically recompiled, measured on the engine a page has.
#
# Both programs here are that, and they are different shapes:
#
#   perl     3.7 MB of interpreter reached through the guest's own ld.so -- a
#            dispatch loop over a switch with a hundred arms, behind a function
#            pointer, with the hot data two pointer hops away. The shape the
#            microbenchmarks do not cover, and the one where a block-per-
#            function translation is worst.
#
#   openssl  hand-written AArch64 with NEON in it, in a shared library, called
#            through the PLT. `dgst` rather than `speed`: `speed` runs until
#            SIGALRM and so does the same amount of *time* however fast the
#            code is, which measures nothing.
#
# Same methodology as everything else here: minimum CPU time of the run phase,
# paired per round, median of the per-round ratios (see bench/ab.mjs).
#
# `GEX_BENCH_ENV_A` / `GEX_BENCH_ENV_B` set one `KEY=VALUE` for one side's runs
# only, for a build whose runtime needs to be told something about it.
set -eu

A="${1:?gexc for side A}"
B="${2:?gexc for side B}"
ROOTFS="${3:?a Debian arm64 rootfs}"
WORK="${4:-${TMPDIR:-/tmp}/gex-real-v8}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
REPS="${GEX_BENCH_REPS:-4}"

for tool in "$A" "$B"; do
    [ -x "$tool" ] || { echo "skip: $tool is not there"; exit 0; }
done
[ -f "$ROOT/shims/browser/js/run-node.mjs" ] || { echo "skip: no browser shim"; exit 0; }
mkdir -p "$WORK"

# The workload lives in the rootfs, so the guest can open it by name.
cat > "$ROOTFS/gex-bench.pl" <<'PL'
# Hashes, strings and regexes, which is what perl is for and what its
# interpreter loop spends itself on. Fixed work, no clock.
my %h; my $acc = 0;
for my $i (1 .. 260000) {
    my $k = "key" . ($i * 2654435761 % 100000);
    $h{$k} += $i;
    $acc += length($k);
    $acc += 1 if $k =~ /^key(\d*7)\d?$/;
}
my @ks = sort keys %h;
for my $k (@ks) { $acc += $h{$k} % 97 }
print "n=", scalar(@ks), " acc=$acc\n";
PL

run_one() {
    name="$1"; prog="$2"; shift 2
    [ -x "$ROOTFS$prog" ] || { echo "skip $name: no $prog in the rootfs"; return 0; }
    for side in a b; do
        gexc=$A; [ "$side" = b ] && gexc=$B
        "$gexc" build "$ROOTFS$prog" -o "$WORK/$name.$side.wasm" \
            --no-shared-memory >/dev/null 2>&1
    done
    # The program argument is a *host* path: that is where the ELF is read
    # from to build the image. `--dir` is what the guest then sees.
    out=$(node "$HERE/ab.mjs" --reps "$REPS" --dir "/=$ROOTFS" --cwd / \
        "$WORK/$name.a.wasm" "$WORK/$name.b.wasm" "$ROOTFS$prog" "$@" 2>&1 | tail -1)
    printf '%-9s %s\n' "$name" "$out"
}

printf '%-9s %s\n' PROGRAM 'CPU TIME OF THE RUN PHASE'
run_one perl /usr/bin/perl /gex-bench.pl
# A digest over the interpreter itself: a real file, big enough to matter, and
# one that is certainly in the rootfs.
run_one openssl /usr/bin/openssl dgst -sha512 -r /usr/bin/perl /usr/bin/perl \
    /usr/bin/perl /usr/bin/perl /usr/bin/perl /usr/bin/perl
rm -f "$ROOTFS/gex-bench.pl"
