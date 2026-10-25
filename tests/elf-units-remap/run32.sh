#!/bin/sh
# run.sh's two libraries at one address, built for i386: the load bias of an
# ELFCLASS32 object is found from its own program headers, so each i386
# library gets its function-tier unit (four placements), and each load runs
# its own code.
#
#   tests/elf-units-remap/run32.sh [gexc]    (needs gcc -m32 and an i386 libc)
set -u
export GEX_LOG=${GEX_LOG:-1}
HERE=$(cd "$(dirname "$0")" && pwd)
GEXC=${1:-target/x86/release/gexc}
W=${WORK:-$(mktemp -d)}
mkdir -p "$W"
if ! echo 'int main(void){return 0;}' | gcc -m32 -x c -o "$W/probe" - 2>/dev/null; then
	echo "skip elf-units-remap i386 (no gcc -m32)"
	exit 0
fi
gcc -m32 -O1 -shared -fPIC -DK=3 -DC=1 -o "$W/liba.so" "$HERE/lib.c" || exit 1
gcc -m32 -O1 -shared -fPIC -DK=7 -DC=-2 -o "$W/libb.so" "$HERE/lib.c" || exit 1
gcc -m32 -O1 -o "$W/main" "$HERE/main.c" -ldl || exit 1
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
	placed=$(grep -c 'function tier for .*/lib[ab]\.so' "$W/err.txt")
	if [ "$native" = "$gex" ] && [ "$placed" = 4 ]; then
		echo "ok   elf-units-remap i386 ${flags:-t1}"
	else
		echo "FAIL elf-units-remap i386 ${flags:-t1} (library units placed $placed times, want 4)"
		printf 'native:\n%s\ngex:\n%s\n' "$native" "$gex"; tail -20 "$W/err.txt"; fail=1
	fi
done
exit $fail
