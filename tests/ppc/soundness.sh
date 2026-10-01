#!/bin/sh
# pewasm's soundness corpus (tests/soundness/cases, PEWASM default
# ~/src/noita-wasm/pewasm) on the PowerPC guest under `gexc run`, compared with
# the expected outputs there. The PE cases do not apply. Two cases carry
# per-architecture assembly that has no PowerPC arm upstream; their ppc32
# ports are in tests/ppc/soundness/ and used instead.
#
#   tests/ppc/soundness.sh [GEXC] [case...]
#
# XFAIL here (a gap every gex guest shares, not PowerPC's):
#   sig_segv: no page protection in wasm, so the write that should fault does not.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
gexc=$root/target/ppc/release/gexc
case "${1:-}" in /*|./*) gexc=$1; shift ;; esac
sound=${PEWASM:-$HOME/src/noita-wasm/pewasm}/tests/soundness
out=$root/target/ppc-soundness
mkdir -p "$out"
XFAIL="sig_segv"
pass=0 fail=0 xfail=0
for src in "$sound"/cases/*.c "$sound"/cases/*.cc; do
    n=$(basename "$src"); n=${n%.*}
    [ $# -gt 0 ] && ! echo " $* " | grep -q " $n " && continue
    head -1 "$src" | grep -q '\[pe\]' && continue
    [ -f "$here/soundness/$(basename "$src")" ] && src=$here/soundness/$(basename "$src")
    exe=$out/$n
    case $src in
        *.cc) zig c++ -target powerpc-linux-musleabihf -mcpu=ppc -O2 -static -fno-pic \
                  -Wl,--image-base=0x1000000 -o "$exe" "$src" 2>"$out/$n.build" ;;
        *) "$root/tools/ppc/cc.sh" -O2 -o "$exe" "$src" -lm 2>"$out/$n.build" ;;
    esac || { echo "ERROR $n: build failed (see $out/$n.build)"; fail=$((fail+1)); continue; }
    got=$(timeout 300 "$gexc" run "$exe" 2>"$out/$n.err")
    want=$(cat "$sound/expected/$n.txt")
    if echo " $XFAIL " | grep -q " $n "; then
        if [ "$got" = "$want" ]; then echo "XPASS $n"; fail=$((fail+1)); else echo "XFAIL $n"; xfail=$((xfail+1)); fi
    elif [ "$got" = "$want" ]; then
        echo "PASS  $n"; pass=$((pass+1))
    else
        echo "FAIL  $n"; echo "$got" > "$out/$n.got"; fail=$((fail+1))
    fi
done
echo "soundness: $pass passed, $fail failed, $xfail xfail"
[ $fail -eq 0 ]
