#!/bin/bash
# ELF export providers against translated glibc (docs/elf-providers.md §4):
# each test runs under gexc with GEX_PROVIDERS=0 and with providers on; the
# outputs and exit statuses must be identical, and with providers on the
# libraries' exports must actually have been bound (GEX_PROVIDER_TRACE).
#
#   tests/elf-prov/run.sh GEXC
#
# The tests are x86-64 C programs built here with the host's gcc against the
# host's glibc. Env: CC (default gcc), GEXC_RUN_FLAGS (extra `gexc run` flags).
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
gexc=$(realpath "${1:?usage: tests/elf-prov/run.sh GEXC}")
work=$root/target/elf-prov-test
rm -rf "$work"
mkdir -p "$work"
fail=0
for src in "$here"/*.c; do
    n=$(basename "$src" .c)
    if ! ${CC:-gcc} -O1 -fno-builtin -o "$work/$n" "$src" -lm -pthread; then
        echo "FAIL $n (build)"
        fail=1
        continue
    fi
    for pv in 0 1; do
        GEX_PROVIDERS=$pv GEX_PROVIDER_TRACE=1 timeout 600 "$gexc" run ${GEXC_RUN_FLAGS:-} "$work/$n" > "$work/$n.$pv.out" 2> "$work/$n.$pv.err"
        echo "exit=$?" >> "$work/$n.$pv.out"
    done
    bound=$(grep -c '^gex: provider .* addresses bound' "$work/$n.1.err")
    if [ "$bound" = 0 ]; then
        fail=1
        echo "FAIL $n (no provider bound)"
        tail -5 "$work/$n.1.err"
    elif cmp -s "$work/$n.0.out" "$work/$n.1.out" && grep -q '^exit=0$' "$work/$n.0.out"; then
        echo "ok   $n ($(grep -vc '^exit=' "$work/$n.0.out") lines agree; $(grep '^gex: provider' "$work/$n.1.err" | sed 's/.*: \([0-9]*\) addresses bound/\1/' | paste -sd+ | bc) addresses bound)"
    else
        fail=1
        echo "FAIL $n"
        diff "$work/$n.0.out" "$work/$n.1.out" | head -20
        tail -5 "$work/$n.1.err"
    fi
done
exit $fail
