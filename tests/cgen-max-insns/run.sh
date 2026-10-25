#!/bin/sh
# A function too big for clang's memory stays out of its unit and runs on
# tier-1, with one build-log line. libbig.so's big() is about 3,000
# instructions of calls and branches, made here; flat() is as long but
# straight-line and trap-free, called by the trap-free flatcall(); small()
# is small. Three builds, each run against the host:
#  - `--max-insns 1000`: big() and flat() are left out before clang runs;
#  - no limits: the unit takes them;
#  - `--clang-mem-mb M`: clang runs out of memory on their units, and the
#    retry leaves each unit's largest function out until the rest compiles.
#    M is found here, stepping up from a floor: the least limit clang
#    starts under that still has it run out on big()'s or flat()'s unit
#    (what it needs depends on the clang build). With no such limit the
#    case is skipped, and said so.
# The unit holds small() every time.
#
# Not checked: that flat()'s trap-free caller writes everything back before
# calling the stub flat() becomes. A missed write-back shows only when the
# host reads the context during the call, and natively nothing does: flat()
# on tier-1 has no safepoint (no back edge) and no call that leaves.
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
# The retry's limit: the first, stepping up, at which the build succeeds
# and says clang ran out on big()'s or flat()'s unit.
mem=
for m in 150 200 250 300 350 400 450 500 600 700 800; do
	GEX_CGEN_CACHE="$W/probe-$m" GEX_CGEN_MAX_INSNS=0 "$GEXC" cgen-image --elf "$W/libbig.so" --base $BASE --max-insns 0 --clang-mem-mb $m --jobs 2 > "$W/probe-$m.log" 2>&1 || continue
	# Built: clang started. Whether it also ran out decides; a larger
	# limit would only run out less.
	grep "clang ran out of its $m MiB): " "$W/probe-$m.log" | grep -q "$bigat (\|$flatat (" && mem=$m
	break
done
rm -rf "$W"/probe-*/
fail=0
retry="0 $mem clang ran out of its $mem MiB"
if [ -n "$mem" ]; then
	echo "cgen-max-insns: the retry case runs clang under $mem MiB"
else
	retry=
	echo "skip cgen-max-insns retry case: no clang memory limit both starts clang and makes it run out on big()'s or flat()'s unit"
fi
# Each case: its options, and the build-log line naming big() or flat() it
# must have ("" for none).
for case in "1000 8192 over 1000 instructions" "0 0 " ${retry:+"$retry"}; do
	set -- $case
	max=$1 mem=$2; shift 2; line="$*"
	# The run looks for the unit under the key the same limit gives.
	export GEX_CGEN_CACHE="$W/cgen-$max-$mem" GEX_ELF_UNITS=libbig.so GEX_ELF_UNITS_BUILD=0 GEX_CGEN_MAX_INSNS=$max
	log="$W/cgen-$max-$mem.log"
	"$GEXC" cgen-image --elf "$W/libbig.so" --base $BASE --max-insns $max --clang-mem-mb $mem --jobs 2 > "$log" 2>&1 || { cat "$log"; exit 1; }
	said=$(grep "left to tier-1 ($line): " "$log" | grep -c "$bigat (\|$flatat (")
	if [ -n "$line" ]; then ok=$([ "$said" -ge 1 ] && echo yes); else ok=$([ "$(grep -c 'left to tier-1' "$log")" = 0 ] && echo yes); fi
	gex=$(timeout 120 "$GEXC" run --pin "$W/pins.txt" "$W/main" 2> "$W/err-$max-$mem.txt")
	placed=$(grep -c 'function tier for .*/libbig\.so' "$W/err-$max-$mem.txt")
	if [ "$ok" != yes ] || [ "$placed" = 0 ] || [ "$gex" != "$native" ]; then
		echo "FAIL cgen-max-insns (--max-insns $max --clang-mem-mb $mem): left-out lines '${line:-none}' $said, unit placed $placed, run '$gex' vs host '$native'"
		grep "\[cgen\]" "$log" | head -4
		fail=1
	fi
done
[ $fail = 0 ] && echo "ok   cgen-max-insns (big() and flat() on tier-1 under 1000 instructions${mem:+ and after clang ran out of $mem MiB}, in the unit with no limit; runs agree)"
[ -z "${WORK:-}" ] && rm -rf "$W"
exit $fail
