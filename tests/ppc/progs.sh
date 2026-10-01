#!/bin/sh
# Small PowerPC guest programs (signals, threads, atomics, FP), each compared
# against qemu-ppc: tests/ppc/progs.sh [GEXC]
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
gexc=${1:-$root/target/ppc/release/gexc}
out=$root/target/ppc-progs
mkdir -p "$out"
pass=0 fail=0
for src in "$here"/progs/*.c; do
    n=$(basename "$src" .c)
    "$root/tools/ppc/cc.sh" -O2 -o "$out/$n" "$src" -lm || { echo "$n: BUILD FAILED"; fail=$((fail+1)); continue; }
    want=$(qemu-ppc "$out/$n" 2>&1)
    got=$(timeout 300 "$gexc" run "$out/$n" 2>&1)
    if [ "$want" = "$got" ]; then
        echo "$n: ok"; pass=$((pass+1))
    else
        echo "$n: MISMATCH"; echo "--- qemu"; echo "$want"; echo "--- gex"; echo "$got"; fail=$((fail+1))
    fi
done
echo "progs: $pass passed, $fail failed"
[ $fail -eq 0 ]
