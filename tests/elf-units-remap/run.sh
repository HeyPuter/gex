#!/bin/sh
# One library unloaded and another loaded at the same address (main.c), with
# function-tier units for both (GEX_ELF_UNITS): each load must run its own
# code, and a library mapped again must get its unit again -- a unit whose
# code was unmapped is retired, not revived by the next object's mapping
# (nt_cgen::on_retire, ftab::on_retire; the browser's cgen::on_retire).
#
#   tests/elf-units-remap/run.sh [gexc]      (needs gcc; host glibc as the guest's)
set -u
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
export GEX_CGEN_CACHE="$W/cgen" GEX_ELF_UNITS=liba.so,libb.so GEX_ELF_UNITS_BUILD=0
for l in liba libb; do
	"$GEXC" cgen-image --elf "$W/$l.so" --base $BASE --jobs 2 > "$W/$l.cgen.log" 2>&1 || { cat "$W/$l.cgen.log"; exit 1; }
done
native=$("$W/main" "$W")
fail=0
for flags in "" "--fjit"; do
	gex=$(timeout 60 "$GEXC" run $flags --pin "$W/pins.txt" "$W/main" "$W" 2> "$W/err.txt")
	placed=$(grep -c 'function tier for' "$W/err.txt")
	if [ "$native" = "$gex" ] && [ "$placed" = 4 ]; then
		echo "ok   elf-units-remap ${flags:-t1}"
	else
		echo "FAIL elf-units-remap ${flags:-t1} (units placed $placed times, want 4)"
		printf 'native:\n%s\ngex:\n%s\n' "$native" "$gex"; tail -20 "$W/err.txt"; fail=1
	fi
done
exit $fail
