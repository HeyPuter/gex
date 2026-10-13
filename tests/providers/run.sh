#!/bin/bash
# Export providers against the guest's own code (docs/merge-plan.md §9.9.3):
# each test in tests/providers/bin/x86 runs with GEX_PROVIDERS=0 and with
# providers on, and the outputs must be identical. hookodstr is the
# exception: a Detours-style hook on a provided export (see its source).
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
    if [ "$n" = hookodstr ]; then
        # The one intended difference: a hook written over a provided
        # export runs only without providers; with them gex reports it once.
        if tr -d '\r' < "$work/$n.0.out" | grep -q '^hook calls while hooked: 1$' && grep -q '^exit=0$' "$work/$n.0.out" &&
            tr -d '\r' < "$work/$n.1.out" | grep -q '^hook calls while hooked: 0$' && grep -q '^exit=0$' "$work/$n.1.out" &&
            grep -q 'rewrote provided export kernel32.dll!OutputDebugStringA' "$work/$n.1.err"; then
            echo "ok   $n (the hook runs without providers; with them it does not, and is reported)"
        else
            fail=1
            echo "FAIL $n"
            head -5 "$work/$n.0.out" "$work/$n.1.out"
            grep -H rewrote "$work/$n.1.err"
        fi
        continue
    fi
    if cmp -s "$work/$n.0.out" "$work/$n.1.out" && grep -q '^exit=0$' "$work/$n.0.out"; then
        echo "ok   $n ($(grep -vc '^exit=' "$work/$n.0.out") lines agree)"
    else
        fail=1
        echo "FAIL $n"
        diff "$work/$n.0.out" "$work/$n.1.out" | head -20
    fi
done
# crtmath on units: Wine's trigonometry provider (wine_libm, `lifted`)
# falls back to the lifted body kept in its unit for every case it cannot
# match (another control word, NaN and infinity, huge arguments, an x87
# register that is no double), so it is bound only where msvcr120 has a
# unit. Run 1: providers on, no units -- wine_libm left unbound, the
# output the tier-1 one; it logs msvcr120's base. Then msvcr120's unit at
# that base (`cgen-image`, into the work directory's own cache), and run 2
# on it: the same output again, with wine_libm bound and each of its
# exports' lifted twins run (crtmath's rounding-up, 64-bit and x87-stack
# modes reach all 16).
if [ -f "$work/crtmath.0.out" ]; then
    cg=$work/cg
    run_u() {
        (cd "$work" && GEX_CGEN_CACHE=$cg GEX_NT_CGEN_BUILD=0 GEX_PROVIDERS=1 GEX_NT_STRICT=1 timeout 600 "$gexc" run --wine-prefix "$prefix" crtmath.exe \
            > "$work/crtmath.$1.out" 2> "$work/crtmath.$1.err")
        echo "exit=$?" >> "$work/crtmath.$1.out"
    }
    run_u u0
    at=$(grep -ao 'msvcr120.dll at 0x[0-9a-f]*: no function-tier unit' "$work/crtmath.u0.err" | head -1 | grep -o '0x[0-9a-f]*')
    dll=$(grep -ao 'warning: .*msvcr120.dll at 0x[0-9a-f]*: no function-tier unit' "$work/crtmath.u0.err" | head -1 | sed 's/^warning: \(.*\) at 0x[0-9a-f]*: .*/\1/')
    if [ -z "$at" ] || ! grep -q 'drive_c/windows' <<< "$dll"; then
        echo "skip crtmath on units (msvcr120 is not the prefix's: ${dll:-none})"
    else
        GEX_CGEN_CACHE=$cg "$gexc" cgen-image "$dll" --pe-base "$at" --jobs "${JOBS:-8}" > "$work/cgen.log" 2>&1
        run_u u1
        fb=$(grep -c 'fell back to its lifted body$' "$work/crtmath.u1.err")
        if cmp -s "$work/crtmath.0.out" "$work/crtmath.u0.out" && cmp -s "$work/crtmath.0.out" "$work/crtmath.u1.out" &&
            grep -q 'provider wine_libm for .*msvcr120.dll: 16 exports' "$work/crtmath.u1.err" && [ "$fb" -ge 16 ]; then
            echo "ok   crtmath on units (msvcr120 at $at: wine_libm bound, $fb of its exports fell back to lifted twins; $(grep -vc '^exit=' "$work/crtmath.u1.out") lines agree)"
        else
            fail=1
            echo "FAIL crtmath on units (msvcr120 at $at; $fb exports fell back)"
            diff "$work/crtmath.0.out" "$work/crtmath.u0.out" | head -5
            diff "$work/crtmath.0.out" "$work/crtmath.u1.out" | head -10
            grep -a 'wine_libm\|panicked\|error' "$work/crtmath.u1.err" | head -5
        fi
    fi
fi
exit $fail
