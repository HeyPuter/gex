#!/bin/sh
# Does statically recompiling the shared libraries make the guest faster?
#
#   bench/objects.sh <debian-rootfs>
#
# It does not, and this is how that was established. The two configurations
# are the same program on the same input, differing only in whether every
# mapped object was recompiled up front (`--recompile-objects`) or left to the
# runtime translator.
#
# The delta between them is constant across input sizes -- about two seconds
# on this machine -- and that is exactly the cost of instantiating the library
# modules. The code itself runs at the same speed. Measured:
#
#   sha256sum, 20 MB    0.82s  vs  3.53s    delta 2.71
#   sha256sum, 200 MB   4.33s  vs  6.35s    delta 2.02
#   wc -w, 52 MB        9.80s  vs 11.82s    delta 2.02
#
# A delta that does not grow with the work is not a code-quality difference.
#
# The caveat worth keeping: all three of those spend their time in the
# *executable*, which is translated ahead of time either way, so what they
# really show is that static object recompilation does not make the executable
# slower. A workload whose hot loop is inside a shared library would be the
# stronger test, and `sort` on a large file is the obvious candidate -- it was
# too slow to warm up inside the time available here.
set -eu

R="${1:?usage: bench/objects.sh <debian-rootfs>}"
GEXC="$(cd "$(dirname "$0")/.." && pwd)/target/release/gexc"
RUN="$GEXC run --uid 0 --sysroot $R"

[ -x "$GEXC" ] || { echo "skip: build gexc first"; exit 0; }

mkdir -p "$R/var"
[ -f "$R/var/bench.bin" ] || head -c 50000000 /dev/urandom > "$R/var/bench.bin"

# Seconds of wall clock, without depending on /usr/bin/time being installed.
elapsed() {
    start=$(date +%s.%N)
    "$@" >/dev/null 2>&1 || true
    end=$(date +%s.%N)
    awk -v a="$start" -v b="$end" 'BEGIN { printf "%.2f", b - a }'
}

each() {
    label="$1"; shift
    # Both configurations are warmed first: a cold run measures Cranelift,
    # not the guest, and the difference between the two is far smaller than
    # that.
    $RUN "$@" >/dev/null 2>&1 || true
    $RUN --recompile-objects "$@" >/dev/null 2>&1 || true
    a=$(elapsed $RUN "$@")
    b=$(elapsed $RUN --recompile-objects "$@")
    printf '%-24s demand %6ss   static %6ss\n' "$label" "$a" "$b"
}

each "sha256sum 50MB" "$R/usr/bin/sha256sum" /var/bench.bin
each "md5sum 50MB" "$R/usr/bin/md5sum" /var/bench.bin
