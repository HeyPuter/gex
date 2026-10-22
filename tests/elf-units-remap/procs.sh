#!/bin/bash
# Every process that maps an object listed in GEX_ELF_UNITS runs its unit:
# two.c maps liba.so at a pinned base, forks from the library's own code,
# forks a child that calls into it, and runs another program (other.c) that
# maps it while two.c still has it.
#
#   native  both programs place the unit (a forked child shares its
#           parent's), under --sysroot, so the object is read through the
#           guest's namespace; the pin holds in the forked program too
#   browser all four processes place it: a forked child's shim starts with
#           its parent's list of placed objects but none of their units
#
#   tests/elf-units-remap/procs.sh [gexc]
#
# Needs ~/toolchains/aarch64-linux-musl-cross (GEX_TOOLCHAIN); the browser
# leg needs the shared build in web/public/gex (GEX_SHARED=1
# shims/browser/build.sh) and is skipped without it.
set -u
# The lines checked below ("function tier for") are debug output.
export GEX_LOG=${GEX_LOG:-1}
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
gexc=$(realpath "${1:-$root/target/release/gexc}")
tc=${GEX_TOOLCHAIN:-$HOME/toolchains/aarch64-linux-musl-cross}
cc=$tc/bin/aarch64-linux-musl-gcc
ldso=$tc/aarch64-linux-musl/lib/libc.so
for t in "$gexc" "$cc"; do [ -e "$t" ] || { echo "skip: $t is not there"; exit 0; }; done
w=${WORK:-$root/target/elf-units-procs-test}
rm -rf "$w"; mkdir -p "$w"
"$cc" -O1 -shared -fPIC -DK=3 -DC=1 -o "$w/liba.so" "$here/lib.c" || exit 1
"$cc" -O1 -o "$w/two" "$here/two.c" || exit 1
"$cc" -O1 -o "$w/other" "$here/other.c" || exit 1
BASE=0x30000000
# By guest path, as a closure build writes them (the page's placement table
# for later processes matches the path exactly); natively the guest's path is /t's.
printf '%s 0x10000 /t/liba.so\n' $BASE > "$w/pins.txt"
printf '%s 0x10000 %s\n' $BASE "$w/liba.so" > "$w/pins-web.txt"
export GEX_CGEN_CACHE=$w/cgen GEX_ELF_UNITS=liba.so GEX_ELF_UNITS_BUILD=0
"$gexc" cgen-image --elf "$w/liba.so" --base $BASE --jobs 2 > "$w/cgen.log" 2>&1 || { cat "$w/cgen.log"; exit 1; }
want="parent f(5)=51
forked 3
child f(9)=145
other f(7)=92
other exited 0"
# Processes that placed liba.so's unit, and whether the output is right.
check() { # NAME ERR OUT WANT_PIDS
	local pids
	pids=$(grep -ao 'pid [0-9]*: function tier for [^ ]*/liba\.so' "$2" | sort -u | wc -l)
	if [ "$3" = "$want" ] && [ "$pids" = "$4" ]; then
		echo "ok   elf-units-procs $1 ($pids processes placed liba.so's unit)"
	else
		echo "FAIL elf-units-procs $1 ($pids processes placed liba.so's unit, want $4)"
		printf 'got:\n%s\n' "$3"; grep -a 'gex-elf\|gex-nt\|rror' "$2" | head -10; fail=1
	fi
}
fail=0
# The guest's /t is the host's $w/root/t.
mkdir -p "$w/root/lib" "$w/root/t"
cp "$ldso" "$w/root/lib/ld-musl-aarch64.so.1"
cp "$w/liba.so" "$w/two" "$w/other" "$w/root/t/"
out=$(timeout 120 "$gexc" run --sysroot "$w/root" --pin "$w/pins.txt" "$w/root/t/two" /t 2> "$w/sysroot.err")
check native "$w/sysroot.err" "$out" 2
seed=$root/web/public/gex/seed.shared.wasm
if [ -e "$seed" ]; then
	# The page's units are guarded (its own key).
	GEX_CGEN_GUARD=1 "$gexc" cgen-image --elf "$w/liba.so" --base $BASE --jobs 2 > "$w/cgen-web.log" 2>&1 || { cat "$w/cgen-web.log"; exit 1; }
	# With export providers and without (GEX_PROVIDERS=0): their rescan of a
	# forked child is not what gives it its units.
	for prov in 1 0; do
		out=$(cd "$w" && GEX_PROVIDERS=$prov GEX_SHARED=1 GEX_PINS="$w/pins-web.txt" timeout 300 node "$root/shims/browser/js/run-node.mjs" --cgen-prefer \
			--file "/lib/ld-musl-aarch64.so.1=$ldso" --file "$w/liba.so=$w/liba.so" --file "$w/two=$w/two" \
			--file "$w/other=$w/other" "$seed" "$w/two" "$w" 2> "$w/browser$prov.err" | grep -av '^\[gex\]')
		check "browser providers=$prov" "$w/browser$prov.err" "$out" 4
	done
else
	echo "skip elf-units-procs browser ($seed is not there)"
fi
exit $fail
