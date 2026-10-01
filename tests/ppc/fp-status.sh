#!/bin/sh
# Keeps the FP status gap visible (docs/ppc.md §8.1): fetestexcept(FE_INEXACT)
# after 1.0/3.0 is 1 on the hardware and under qemu-ppc, and 0 under gex,
# which computes no IEEE exception flags on any guest. XFAIL until it does.
#
#   tests/ppc/fp-status.sh [GEXC]    (default: target/ppc/release/gexc)
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
gexc=${1:-$root/target/ppc/release/gexc}
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
"$root/tools/ppc/cc.sh" -O1 "$here/fp-status.c" -o "$out/fp-status" || exit 2
want=$(qemu-ppc "$out/fp-status")
got=$("$gexc" run "$out/fp-status")
echo "qemu-ppc: $want"
echo "gex:      $got"
if [ "$want" = "$got" ]; then
    echo "XPASS: the FP status flags now match; drop the xfail"
    exit 1
fi
if [ "$want" = "inexact=1" ] && [ "$got" = "inexact=0" ]; then
    echo "XFAIL (expected): gex computes no IEEE exception flags (docs/ppc.md §8.1)"
    exit 0
fi
echo "FAIL: unexpected output"
exit 1
