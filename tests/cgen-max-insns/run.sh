#!/bin/sh
# A function too big for clang's memory stays out of its unit and runs on
# tier-1, with one build-log line (`gexc cgen-image --max-insns`): libbig.so's
# big() is a few thousand instructions of calls and branches, made here,
# beside small(), and flat(), as long but straight-line and trap-free, with
# its trap-free caller flatcall(). With a limit below their size the unit
# leaves big() and flat() out and the run still agrees with the host; with
# no limit (0) the unit takes them. The unit holds small() either way.
#
#   tests/cgen-max-insns/run.sh [gexc]      (needs gcc; host glibc as the guest's)
set -u
export GEX_LOG=${GEX_LOG:-1}
HERE=$(cd "$(dirname "$0")" && pwd)
GEXC=${1:-target/x86/release/gexc}
# Not on a tmpfs: the guest reads its libraries through the host.
W=${WORK:-$(mktemp -d)}
mkdir -p "$W"
# big(): N statements, each a call, a compare and a branch over eight live
# locals, the shape of a big parser or table loader.
awk -v n=${N:-250} 'BEGIN {
	print "#include <string.h>";
	print "__attribute__((noinline)) int get(void *p, int k) { return ((long *)p)[k & 7] + k; }";
	print "__attribute__((noinline)) long put(void *p, int k, long v) { ((long *)p)[k & 7] ^= v; return v + k; }";
	print "static const char *names[4] = {\"k1\", \"k2\", \"k3\", \"k4\"};";
	print "__attribute__((noinline)) const char *name(void *p, int k) { return names[(((long *)p)[k & 7] + k) & 3]; }";
	print "long small(long x) { return x * 3 + 1; }";
	# flat(): straight-line arithmetic, no call, no loop (trap-free), and
	# its trap-free caller flatcall(): under the limit flat() leaves, so
	# flatcall() must write back before calling it.
	print "__attribute__((noinline, visibility(\"hidden\"))) long flat(long x, long y) {";
	for (k = 0; k < 400; k++) printf "  x ^= x << %d; y += x >> %d; x += y * %d;\n", k % 29 + 1, k % 17 + 1, k % 11 + 2;
	print "  return x ^ y;";
	print "}";
	print "long flatcall(long x) { return flat(x, x + 7) + 1; }";
	print "long big(void *p, long *a) {";
	print "  long x0 = a[0], x1 = a[1], x2 = a[2], x3 = a[3], x4 = a[4], x5 = a[5], x6 = a[6], x7 = a[7];";
	srand(2);
	for (k = 0; k < n; k++) {
		v = "x" int(rand() * 8); w = "x" int(rand() * 8); r = rand();
		if (r < 0.4) printf "  if (strcmp(name(p, %d), \"k%d\") == 0) %s = put(p, %d, %s + %d);\n", k, k % 4 + 1, v, k, w, k;
		else if (r < 0.7) printf "  if (get(p, %d) > %d) %s += %s * %d; else %s ^= put(p, %d, %s);\n", k, int(rand() * 100), v, w, k % 13 + 1, w, k, v;
		else printf "  %s = %s + get(p, %d) * %d;\n", v, w, k, k % 7 + 1;
	}
	print "  return x0 + x1 + x2 + x3 + x4 + x5 + x6 + x7;";
	print "}";
}' > "$W/big.c"
gcc -O2 -shared -fPIC -o "$W/libbig.so" "$W/big.c" || exit 1
gcc -O1 -o "$W/main" "$HERE/main.c" -L"$W" -lbig -Wl,-rpath,'$ORIGIN' || exit 1
BASE=0x30000000
printf '%s 0x10000 libbig.so\n' $BASE > "$W/pins.txt"
at() { printf '%#x' $(( BASE + 0x$(nm "$W/libbig.so" | awk -v s=$1 '$3 == s { print $1 }') )); }
bigat=$(at big) flatat=$(at flat)
native=$("$W/main")
fail=0
for max in 1000 0; do
	# The run looks for the unit under the key the same limit gives.
	export GEX_CGEN_CACHE="$W/cgen-$max" GEX_ELF_UNITS=libbig.so GEX_ELF_UNITS_BUILD=0 GEX_CGEN_MAX_INSNS=$max
	"$GEXC" cgen-image --elf "$W/libbig.so" --base $BASE --max-insns $max --jobs 2 > "$W/cgen-$max.log" 2>&1 || { cat "$W/cgen-$max.log"; exit 1; }
	left=$(grep "left to tier-1 (over $max instructions): " "$W/cgen-$max.log" | grep "$bigat (" | grep -c "$flatat (")
	want=1; [ $max = 0 ] && want=0
	gex=$(timeout 120 "$GEXC" run --pin "$W/pins.txt" "$W/main" 2> "$W/err-$max.txt")
	placed=$(grep -c 'function tier for .*/libbig\.so' "$W/err-$max.txt")
	if [ "$left" != "$want" ] || [ "$placed" = 0 ] || [ "$gex" != "$native" ]; then
		echo "FAIL cgen-max-insns (limit $max): big() and flat() left out $left (want $want), unit placed $placed, run '$gex' vs host '$native'"
		grep "\[cgen\]" "$W/cgen-$max.log" | head -3
		fail=1
	fi
done
[ $fail = 0 ] && echo "ok   cgen-max-insns (big() at $bigat and flat() at $flatat on tier-1 under a limit of 1000 instructions, in the unit with none; runs agree)"
[ -z "${WORK:-}" ] && rm -rf "$W"
exit $fail
