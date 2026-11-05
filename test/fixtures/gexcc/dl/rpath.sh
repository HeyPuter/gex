#!/bin/sh
# gexcc-rpath: a module's run path (dylink.0), rewritten in place as an
# install step does an ELF's. A side module that needs another library, and
# a program that needs it, are linked with a build tree's run path, which
# names no directory once installed: loading them fails; rewritten to
# where the library is, they load. The file keeps its size, and a longer
# run path than the old one is refused.
#
#   test/fixtures/gexcc/dl/rpath.sh DIR    (gexcc on PATH; DIR: where gex can see,
#                                not /dev/shm)
#   BROWSER=ROOT: the loads in the browser runtime of gex checkout ROOT
#   (hosts/browser/js/run-node.mjs, the shim and seed in its page/public/gex;
#   GEX_ARCH=x86 for the x86 build's)
set -u
command -v gexcc > /dev/null || { echo "gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 2; }
W=$(mktemp -d "${1:?DIR}/rpath.XXXXXX")
trap 'rm -rf "$W"' EXIT
mkdir -p "$W/lib/a" "$W/bin"
cat > "$W/b.c" <<'X'
int b(void) { return 42; }
X
cat > "$W/a.c" <<'X'
int b(void);
int a(void) { return b() + 1; }
X
cat > "$W/m.c" <<'X'
#include <dlfcn.h>
#include <stdio.h>
int main(int argc, char **argv) {
    void *h = dlopen(argv[1], RTLD_NOW);
    if (!h) { printf("no dlopen: %s\n", dlerror()); return 1; }
    int (*a)(void) = (int (*)(void))dlsym(h, "a");
    printf("%s\n", a && a() == 43 ? "ok" : "FAIL call");
    return 0;
}
X
cat > "$W/n.c" <<'X'
#include <stdio.h>
int a(void);
int main(void) { printf("%s\n", a() == 43 ? "ok" : "FAIL call"); return 0; }
X
pad='$ORIGIN/build/XXXXXXXXXXXXXXXX'
GEXCC_SHARED=1 gexcc -fPIC -shared -o "$W/lib/libb.so" "$W/b.c" &&
    GEXCC_SHARED=1 gexcc -fPIC -shared -o "$W/lib/a/liba.so" "$W/a.c" -L"$W/lib" -lb -Wl,-rpath,"$pad" &&
    GEXCC_SHARED=1 gexcc -o "$W/bin/m" "$W/m.c" &&
    GEXCC_SHARED=1 gexcc -o "$W/bin/n" "$W/n.c" -L"$W/lib/a" -la -Wl,-rpath,"$pad" || { echo "FAIL build"; exit 1; }
fail=0
if [ -n "${BROWSER:-}" ]; then
    case ${GEX_ARCH:-aarch64} in x86) infix=.x86 ;; *) infix= ;; esac
    libc=$(dirname "$(command -v gexcc)")/../sysroot/lib/libc.so
    run() { (GEX_SHARED=1 GEX_LOG=off timeout 300 node "$BROWSER/hosts/browser/js/run-node.mjs" --dir "$W=$W" \
        --file "/usr/lib/wasm32-linux-musl/libc.so=$libc" "$BROWSER/page/public/gex/seed$infix.shared.wasm" "$@" 2>&1) |
        grep -v '^\[host\]\|^\[worker\] \|^\[gex-console' | tail -1; }
else
    run() { (cd "$W" && gexcc-exec "$@" 2>&1) | tail -1; }
fi
[ "$(gexcc-rpath "$W/lib/a/liba.so")" = "$pad" ] || { echo "FAIL rpath: the side module's run path is not the one linked"; fail=1; }
[ "$(run "$W/bin/m" "$W/lib/a/liba.so")" != ok ] || { echo "FAIL rpath: the side module loaded with a run path naming no directory"; fail=1; }
s=$(wc -c < "$W/lib/a/liba.so")
gexcc-rpath --set '$ORIGIN/..' "$W/lib/a/liba.so" || fail=1
[ "$(gexcc-rpath "$W/lib/a/liba.so")" = '$ORIGIN/..' ] || { echo "FAIL rpath: the side module's run path was not replaced"; fail=1; }
[ "$(wc -c < "$W/lib/a/liba.so")" = "$s" ] || { echo "FAIL rpath: the side module changed size"; fail=1; }
r=$(run "$W/bin/m" "$W/lib/a/liba.so"); [ "$r" = ok ] || { echo "$r"; echo "FAIL rpath: the rewritten side module did not load"; fail=1; }
# the program, in its container: its own run path finds liba.so, whose finds libb.so
gexcc-rpath --set '$ORIGIN/../lib/a' "$W/bin/n" || fail=1
r=$(run "$W/bin/n"); [ "$r" = ok ] || { echo "$r"; echo "FAIL rpath: the rewritten program did not load"; fail=1; }
if gexcc-rpath --set "$pad/and/longer" "$W/bin/n" 2>/dev/null; then echo "FAIL rpath: a longer run path was taken"; fail=1; fi
[ $fail = 0 ] && echo "ok rpath" || { echo "FAIL rpath"; exit 1; }
