#!/bin/sh
# gexcc-libcbuiltins: compiler-rt's builtins that libc.so carries are
# exported to the libraries it serves. A side module doing 128-bit and
# long double arithmetic takes __multi3 and the long double helpers from
# libc.so (they are not linked into it); a libc.so whose builtins were built
# hidden exports none, and the library does not load.
#
#   test/fixtures/gexcc/dl/libcbuiltins.sh DIR    (gexcc on PATH)
set -u
command -v gexcc > /dev/null || { echo "gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 2; }
W=$(mktemp -d "${1:?DIR}/libcbuiltins.XXXXXX")
trap 'rm -rf "$W"' EXIT
libc=$(dirname "$(command -v gexcc)")/../sysroot/lib/libc.so
fail=0
for s in __multi3 __ashlti3 __lshrti3 __addtf3 __multf3 __divtf3 __trunctfdf2 __extenddftf2 __fixtfsi __floatsitf; do
    llvm-nm --extern-only --defined-only "$libc" | grep -q " $s\$" || { echo "FAIL libc.so exports $s"; fail=1; }
done
[ $fail = 1 ] || echo "ok libc.so exports compiler-rt's builtins"
cat > "$W/w.c" <<'X'
unsigned __int128 wide(unsigned long long a, unsigned long long b) { return (unsigned __int128)a * b; }
long double half(long double x, int n) { return x / n + 1.0L; }
int top(unsigned long long a, unsigned long long b) { return (int)(wide(a, b) >> 64); }
double ld(double x) { return (double)half(x, 2); }
X
cat > "$W/m.c" <<'X'
#include <stdio.h>
int top(unsigned long long, unsigned long long);
double ld(double);
int main(void) {
    printf("%s\n", top(1ULL << 40, 1ULL << 30) == 64 && ld(5.0) == 3.5 ? "ok" : "FAIL values");
    return 0;
}
X
GEXCC_SHARED=1 gexcc -O1 -fPIC -shared -o "$W/libw.so" "$W/w.c" &&
    GEXCC_SHARED=1 gexcc -o "$W/m" "$W/m.c" -L"$W" -lw -Wl,-rpath,'$ORIGIN' || { echo "FAIL build"; exit 1; }
out=$(cd "$W" && gexcc-exec "$W/m" 2>&1 | tail -1)
[ "$out" = ok ] && echo "ok a library takes 128-bit and long double builtins from libc.so" || { echo "FAIL a library takes 128-bit and long double builtins from libc.so: $out"; fail=1; }
exit $fail
