#!/bin/bash
# Export providers against the guest's own code (docs/merge-plan.md §9.9.3):
# each test in tests/providers/bin/x86 runs with GEX_PROVIDERS=0 and with
# providers on, and the outputs must be identical.
#
#   tests/providers/run.sh GEXC [DLL...]
#
# Each DLL given (e.g. Noita's msvcr120.dll, the one Noita runs) is copied
# beside the tests, so they load it instead of the staged prefix's Wine
# build; with none, the prefix's are tested. --no-cgen, so the guest's own
# code is tier-1 and needs no units. Env: GEX_WINE_PREFIX.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
gexc=$(realpath "${1:?usage: tests/providers/run.sh GEXC [DLL...]}")
shift
prefix=${GEX_WINE_PREFIX:-$HOME/.cache/gex/wine-11.15}
work=$root/target/providers-test
rm -rf "$work"
mkdir -p "$work"
cp "$here"/bin/x86/*.exe "$work/"
for d in "$@"; do cp "$d" "$work/"; done
fail=0
for exe in "$work"/*.exe; do
    n=$(basename "$exe" .exe)
    for pv in 0 1; do
        (cd "$work" && GEX_PROVIDERS=$pv GEX_NT_STRICT=1 timeout 600 "$gexc" run --no-cgen --wine-prefix "$prefix" "$n.exe" \
            > "$work/$n.$pv.out" 2> "$work/$n.$pv.err")
        echo "exit=$?" >> "$work/$n.$pv.out"
    done
    if cmp -s "$work/$n.0.out" "$work/$n.1.out" && grep -q '^exit=0$' "$work/$n.0.out"; then
        echo "ok   $n ($(grep -vc '^exit=' "$work/$n.0.out") lines agree)"
    else
        fail=1
        echo "FAIL $n"
        diff "$work/$n.0.out" "$work/$n.1.out" | head -20
    fi
done
exit $fail
