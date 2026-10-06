#!/bin/bash
# tests/elf-prov in the browser runtime (shims/browser/js/run-node.mjs): each
# program (built by tests/elf-prov/run.sh into target/elf-prov-test) runs with
# GEX_PROVIDERS=0 and with providers on; the outputs must match and the
# providers must have been bound. The provider modules come from the unit
# cache (GEX_CGEN_CACHE, default ~/.cache/gex/cgen), where a native run puts
# them; the guest's ld.so and libc are the host's.
#
#   tests/elf-prov/run-browser.sh GEXC
#
# Needs `GEX_ARCH=x86 GEX_SHARED=1 shims/browser/build.sh`.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
gexc=$(realpath "${1:?usage: tests/elf-prov/run-browser.sh GEXC}")
work=$root/target/elf-prov-test
libs=""
for f in /lib64/ld-linux-x86-64.so.2 /lib/x86_64-linux-gnu/libc.so.6 /lib/x86_64-linux-gnu/libm.so.6; do libs="$libs --file $f=$(realpath "$f")"; done
fail=0
for src in "$here"/*.c; do
    n=$(basename "$src" .c)
    if [ ! -x "$work/$n" ] || ! "$gexc" build "$work/$n" -o "$work/$n.wasm" > /dev/null 2>&1; then
        echo "FAIL $n (no program: run tests/elf-prov/run.sh first)"
        fail=1
        continue
    fi
    for pv in 0 1; do
        # shellcheck disable=SC2086
        (cd "$work" && GEX_PROVIDERS=$pv GEX_ARCH=x86 GEX_SHARED=1 timeout "${TMO:-900}" node "$root/shims/browser/js/run-node.mjs" --cgen-prefer \
            --file "$work/$n=$work/$n" $libs "$work/$n.wasm" "$work/$n" > "$work/$n.b$pv.out" 2> "$work/$n.b$pv.err")
        echo "exit=$?" >> "$work/$n.b$pv.out"
    done
    grep -av '^\[' "$work/$n.b0.out" > "$work/$n.b0.cmp"
    grep -av '^\[' "$work/$n.b1.out" > "$work/$n.b1.cmp"
    b1=$(grep -c 'addresses bound' "$work/$n.b1.err")
    b0=$(grep -c 'addresses bound' "$work/$n.b0.err")
    if [ "$b1" = 0 ] || [ "$b0" != 0 ]; then
        fail=1
        echo "FAIL $n (bound with providers: $b1 times; without: $b0)"
    elif cmp -s "$work/$n.b0.cmp" "$work/$n.b1.cmp" && grep -q '^exit=0$' "$work/$n.b0.cmp" && cmp -s "$work/$n.b1.cmp" "$work/$n.1.out"; then
        echo "ok   $n ($(grep -vc '^exit=' "$work/$n.b0.cmp") lines agree, and with the native run; bound $b1 times)"
    else
        fail=1
        echo "FAIL $n"
        diff "$work/$n.b0.cmp" "$work/$n.b1.cmp" | head -10
        diff "$work/$n.1.out" "$work/$n.b1.cmp" | head -10
    fi
done
exit $fail
