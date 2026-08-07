#!/bin/sh
# `openssl speed` under gex, against qemu.
#
#   bench/openssl.sh <debian-rootfs> [seconds-per-measurement]
#
# A real benchmark rather than one written for the occasion: it is what
# OpenSSL ships to measure itself, the code being measured is hand-written
# AArch64 with NEON in it, and the shape -- a tight loop over a fixed buffer
# for a fixed wall-clock window -- is the one an emulator is judged on.
#
# It also needed two things gex did not have, which is the more interesting
# half of why it is here.
#
#   `setitimer` was accepted and dropped. `openssl speed` runs each
#   measurement loop until SIGALRM arrives and has no other stopping
#   condition, so a two-second benchmark ran forever.
#
#   `times` returned zeros. openssl divides the byte count by the processor
#   time it reports, so every throughput printed as `infk`.
#
# Measured on this machine, 1000s of bytes per second, two seconds per point:
#
#                   16 B      64 B     256 B    1024 B    8192 B   16384 B
#   sha256   gex   2027.51   9683.68  26511.49  51019.78  87244.80  72507.39
#            qemu  1712.64   5369.60  13712.21  25557.50  30685.01  24461.49
#            ratio    1.18      1.80      1.93      2.00      2.84      2.96
#
#   aes-128  gex  24111.92  43922.05  64452.10  85642.24  87437.31  79093.76
#   -cbc     qemu 12679.28  24123.12  30669.31  29746.69  28294.57  27622.27
#            ratio    1.90      1.82      2.10      2.88      3.09      2.86
#
# Note what is being measured: the crypto lives in libcrypto.so, and a shared
# library is translated on demand rather than ahead of time, so these are
# numbers for the runtime translator. Running with `--recompile-objects` moves
# the same code through the static path and, as everywhere else, does not
# change the throughput -- only how long the first measurement takes.
#
# Which is the one caveat to read the output with. The first block size
# measured pays for translating the code it is measuring: on a cold cache the
# 16-byte sha256 figure comes out at 13.68, four orders of magnitude low, and
# is meaningless. The script warms the cache first. If a number looks absurd
# and it is the first one, that is what happened.
set -eu

R="${1:?usage: bench/openssl.sh <debian-rootfs> [seconds]}"
SECS="${2:-2}"
HERE="$(cd "$(dirname "$0")/.." && pwd)"
GEXC="$HERE/target/release/gexc"
QEMU="${GEX_QEMU:-$HOME/toolchains/qemu-aarch64}"
SSL=/usr/bin/openssl
ALGS="${ALGS:-sha256 aes-128-cbc}"

[ -x "$GEXC" ] || { echo "skip: build gexc first"; exit 0; }
[ -x "$R$SSL" ] || { echo "skip: no $SSL in $R"; exit 0; }

# Translate everything the benchmark will touch before timing anything.
"$GEXC" run --uid 0 --sysroot "$R" "$R$SSL" speed -seconds 1 $ALGS >/dev/null 2>&1 || true

echo "== gex =="
"$GEXC" run --uid 0 --sysroot "$R" "$R$SSL" speed -seconds "$SECS" $ALGS 2>/dev/null |
    grep -E '^(type|[a-z0-9]+ +[0-9])'
echo "== qemu =="
"$QEMU" -L "$R" "$R$SSL" speed -seconds "$SECS" $ALGS 2>/dev/null |
    grep -E '^(type|[a-z0-9]+ +[0-9])'
