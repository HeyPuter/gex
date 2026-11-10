#!/bin/sh
# gexcc-rlibctors: a Rust rlib's constructors are linked, as rustc's ELF
# links have them. A crate's registration (inventory's submit!, a ctor) is
# a constructor in an object nothing else refers to; rustc pulls those in
# on ELF through a symbols.o it does not write for wasm, so gexcc links an
# rlib's members that have constructors whole. Here the rlib is made as
# rustc makes one (an ar archive of wasm objects); the same archive named
# .a keeps the archive rule (C: an unreferenced member stays out). Static
# and shared (GEXCC_SHARED) programs.
#
#   test/fixtures/gexcc/dl/rlibctors.sh DIR    (gexcc on PATH)
set -u
command -v gexcc > /dev/null || { echo "gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 2; }
W=$(mktemp -d "${1:?DIR}/rlibctors.XXXXXX")
trap 'rm -rf "$W"' EXIT
cat > "$W/reg.c" <<'X'
const char *registered[8];
int nregistered;
void submit(const char *s) { registered[nregistered++] = s; }
X
cat > "$W/sub.c" <<'X'
void submit(const char *);
__attribute__((constructor)) static void sub_ctor(void) { submit("from-sub"); }
int sub_unused(void) { return 7; }
X
cat > "$W/m.c" <<'X'
#include <stdio.h>
extern const char *registered[8];
extern int nregistered;
void submit(const char *);
__attribute__((constructor)) static void main_ctor(void) { submit("from-main"); }
int main(void) {
    for (int i = 0; i < nregistered; i++) printf("%s%s", i ? " " : "", registered[i]);
    printf("\n");
    return 0;
}
X
gexcc -O1 -fPIC -c "$W/sub.c" -o "$W/sub.o" && gexcc -O1 -fPIC -c "$W/reg.c" -o "$W/reg.o" &&
    llvm-ar crs "$W/libsub.rlib" "$W/sub.o" && cp "$W/libsub.rlib" "$W/libsub.a" || { echo "FAIL build"; exit 1; }
fail=0
for sh in 0 1; do
    what=static; [ $sh = 1 ] && what=shared
    for lib in libsub.rlib libsub.a; do
        GEXCC_SHARED=$sh gexcc -O1 -o "$W/m-$sh-$lib" "$W/m.c" "$W/reg.o" "$W/$lib" || { echo "FAIL $what link with $lib"; fail=1; continue; }
        out=$(cd "$W" && gexcc-exec "$W/m-$sh-$lib" 2>&1 | tail -1)
        case $lib in
        *.rlib) want="from-sub from-main"; alt="from-main from-sub"; say="an rlib's constructor is linked" ;;
        *) want="from-main"; alt=; say="an archive's unreferenced member stays out" ;;
        esac
        if [ "$out" = "$want" ] || { [ -n "$alt" ] && [ "$out" = "$alt" ]; }; then echo "ok $what: $say"; else echo "FAIL $what: $say: got '$out'"; fail=1; fi
    done
done
exit $fail
