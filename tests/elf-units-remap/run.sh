#!/bin/sh
# One library unloaded and another loaded at the same address (main.c), with
# function-tier units for both (GEX_ELF_UNITS): each load must run its own
# code, and a library mapped again must get its unit again -- a unit whose
# code was unmapped is retired, not revived by the next object's mapping
# (pe_units::on_retire, ftab::on_retire; the browser's cgen::on_retire).
# The interpreter has a unit too: gex's loader places it, and the kernel
# announces it as the guest's own mmap would have (CodeMap::of_placed).
#
#   tests/elf-units-remap/run.sh [gexc]      (needs gcc; host glibc as the guest's)
set -u
# The lines checked below ("function tier for", "replaced by") are debug output.
export GEX_LOG=${GEX_LOG:-1}
HERE=$(cd "$(dirname "$0")" && pwd)
GEXC=${1:-target/x86/release/gexc}
W=${WORK:-$(mktemp -d)}
mkdir -p "$W"
gcc -O1 -shared -fPIC -DK=3 -DC=1 -o "$W/liba.so" "$HERE/lib.c" || exit 1
gcc -O1 -shared -fPIC -DK=7 -DC=-2 -o "$W/libb.so" "$HERE/lib.c" || exit 1
gcc -O1 -o "$W/main" "$HERE/main.c" -ldl || exit 1
# Both at one pinned base, so the units built for it apply.
BASE=0x30000000
printf '%s 0x10000 liba.so\n%s 0x10000 libb.so\n' $BASE $BASE > "$W/pins.txt"
INTERP=$(readelf -lW "$W/main" | sed -n 's/.*Requesting program interpreter: \(.*\)]/\1/p')
export GEX_CGEN_CACHE="$W/cgen" GEX_ELF_UNITS=liba.so,libb.so,${INTERP##*/} GEX_ELF_UNITS_BUILD=0
for l in liba libb; do
	"$GEXC" cgen-image --elf "$W/$l.so" --base $BASE --jobs 2 > "$W/$l.cgen.log" 2>&1 || { cat "$W/$l.cgen.log"; exit 1; }
done
# At gex_abi::INTERP_BASE.
"$GEXC" cgen-image --elf "$INTERP" --base 0x4a00000 --jobs 2 > "$W/interp.cgen.log" 2>&1 || { cat "$W/interp.cgen.log"; exit 1; }
native=$("$W/main" "$W")
fail=0
for flags in "" "--fjit"; do
	gex=$(timeout 60 "$GEXC" run $flags --pin "$W/pins.txt" "$W/main" "$W" 2> "$W/err.txt")
	placed=$(grep -c 'function tier for .*/lib[ab]\.so' "$W/err.txt")
	interp=$(grep -c "function tier for $INTERP at 0x4a00000" "$W/err.txt")
	if [ "$native" = "$gex" ] && [ "$placed" = 4 ] && [ "$interp" = 1 ]; then
		echo "ok   elf-units-remap ${flags:-t1}"
	else
		echo "FAIL elf-units-remap ${flags:-t1} (library units placed $placed times, want 4; the interpreter's $interp, want 1)"
		printf 'native:\n%s\ngex:\n%s\n' "$native" "$gex"; tail -20 "$W/err.txt"; fail=1
	fi
done
exit $fail
