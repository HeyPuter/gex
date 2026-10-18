#!/bin/sh
# A program exec'd by a relative path (`./prog`) is named absolutely by
# `/proc/self/exe`, as Linux records it, under the browser runtime.
#
# ld.so works out `$ORIGIN` from that link and asserts that it starts with
# `/`: Warfork's Linux launcher execs `./wf_steam.x86_64` from its own
# directory, and the page stopped at `_dl_get_origin: Assertion
# `linkval[0] == '/'' failed`. Both ways in: a fork and execv, and
# posix_spawn (the vfork path).
#
#   tests/browser-exec-relative.sh [a64|x86]
#
# Needs zig, node, the guest's gexc, and the shared shim and kernel in
# web/public/gex (shims/browser/build.sh).
set -eu
ARCH="${1:-a64}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${TMPDIR:-/tmp}/gex-exec-relative-$ARCH-$$"
case "$ARCH" in
a64) TARGET=aarch64-linux-musl; GEXC="${GEXC:-$ROOT/target/release/gexc}"; INFIX="" ;;
x86) TARGET=x86_64-linux-musl; GEXC="${GEXC:-$ROOT/target/x86/release/gexc}"; INFIX=".x86"; export GEX_ARCH=x86 ;;
*) echo "usage: $0 [a64|x86]" >&2; exit 2 ;;
esac
for need in "$GEXC" "$ROOT/web/public/gex/gex_browser$INFIX.shared.wasm" "$ROOT/web/public/gex/gex_kernel$INFIX.wasm"; do
    [ -e "$need" ] || { echo "skip: $need is not built" >&2; exit 0; }
done
command -v zig >/dev/null || { echo "skip: no zig to build the guest" >&2; exit 0; }
mkdir -p "$WORK"
[ -n "${KEEP:-}" ] || trap 'rm -rf "$WORK"' EXIT

cat > "$WORK/er.c" <<'EOF2'
#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>
extern char **environ;
static void link_of(char *b, size_t cap) {
    ssize_t n = readlink("/proc/self/exe", b, cap - 1);
    b[n > 0 ? n : 0] = 0;
}
/* er DIR NAME: chdir to DIR and run ./NAME both ways; er WHO: say how
 * /proc/self/exe names this program. */
int main(int argc, char **argv) {
    char me[512], rel[600];
    pid_t p;
    setvbuf(stdout, 0, _IONBF, 0);
    link_of(me, sizeof me);
    if (argc == 2) {
        printf("%s: %s\n", argv[1], me[0] == '/' ? "absolute" : me);
        return 0;
    }
    if (argc != 3 || chdir(argv[1])) { printf("usage: er DIR NAME\n"); return 1; }
    snprintf(rel, sizeof rel, "./%s", argv[2]);
    p = fork();
    if (p == 0) { execl(rel, rel, "execv", (char *)0); _exit(127); }
    waitpid(p, 0, 0);
    char *av[] = { rel, "posix_spawn", 0 };
    if (posix_spawn(&p, rel, 0, 0, av, environ) == 0) waitpid(p, 0, 0);
    else printf("posix_spawn failed\n");
    return 0;
}
EOF2
zig cc -target "$TARGET" -O2 -static -Wl,--image-base=0x1000000 -o "$WORK/er" "$WORK/er.c"
"$GEXC" seed -o "$WORK/seed.wasm" >/dev/null 2>&1
want="execv: absolute
posix_spawn: absolute"
GEX_SHARED=1 timeout "${TMO:-120}" node "$ROOT/shims/browser/js/run-node.mjs" --file /g/er="$WORK/er" "$WORK/seed.wasm" "$WORK/er" /g er > "$WORK/out" 2> "$WORK/err" || true
got=$(grep -v '^\[' "$WORK/out" || true)
if [ "$got" = "$want" ]; then
    echo "ok: a program exec'd as ./prog has an absolute /proc/self/exe, by execv and by posix_spawn ($ARCH)"
    exit 0
fi
echo "FAIL: /proc/self/exe after a relative exec ($ARCH): got" >&2
echo "$got" >&2
tail -5 "$WORK/err" >&2
exit 1
