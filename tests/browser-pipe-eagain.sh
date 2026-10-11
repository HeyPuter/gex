#!/bin/sh
# Blocking pipe writes under the browser runtime never see EAGAIN.
#
# `yes | head -c 20M | wc -c` in a guest, ROUNDS times in one run-node
# process tree: a producer writing as fast as it can, a "head" copying
# exactly N bytes (alternating write and writev), and a counter. Each round
# runs the pipeline once with one head and once with two heads writing into
# the same pipe, where another Worker's writer can fill the pipe between the
# runtime's room check and the write. A head that is told EAGAIN on its
# blocking descriptor exits 3 (7 for a read: two heads also share the
# input pipe), and the count comes up short. SP saw it once
# in the page: 65536 instead of 20000000.
#
#   tests/browser-pipe-eagain.sh [a64|x86] [rounds] [MiB]
#
# Needs zig, node, the guest's gexc, and the shared shim and kernel in
# web/public/gex (shims/browser/build.sh).
set -eu
ARCH="${1:-a64}"
ROUNDS="${2:-8}"
MIB="${3:-20}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${TMPDIR:-/tmp}/gex-pipe-eagain-$ARCH-$$"
case "$ARCH" in
a64) TARGET=aarch64-linux-musl; GEXC="${GEXC:-$ROOT/target/release/gexc}"; INFIX="" ;;
x86) TARGET=x86_64-linux-musl; GEXC="${GEXC:-$ROOT/target/x86/release/gexc}"; INFIX=".x86"; export GEX_ARCH=x86 ;;
*) echo "usage: $0 [a64|x86] [rounds] [MiB]" >&2; exit 2 ;;
esac
for need in "$GEXC" "$ROOT/web/public/gex/gex_browser$INFIX.shared.wasm" "$ROOT/web/public/gex/gex_kernel$INFIX.wasm"; do
    [ -e "$need" ] || { echo "skip: $need is not built" >&2; exit 0; }
done
command -v zig >/dev/null || { echo "skip: no zig to build the guest" >&2; exit 0; }
mkdir -p "$WORK"
[ -n "${KEEP:-}" ] || trap 'rm -rf "$WORK"' EXIT

cat > "$WORK/pe.c" <<'EOF'
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>
#include <unistd.h>
#include <sys/uio.h>
#include <sys/wait.h>
static char buf[1 << 16];
/* yes: until the reader goes. */
static void producer(int out) {
    for (size_t i = 0; i < sizeof buf; i += 2) memcpy(buf + i, "y\n", 2);
    for (;;) if (write(out, buf, sizeof buf) < 0) _exit(errno == EPIPE ? 0 : 4);
}
/* head -c n: exactly n bytes from in to out; 3 on EAGAIN. */
static void head(int in, int out, long n) {
    long left = n; int k = 0;
    while (left > 0) {
        ssize_t r = read(in, buf, left < (long)sizeof buf ? (size_t)left : sizeof buf);
        if (r <= 0) _exit(r == 0 ? 5 : errno == EAGAIN ? 7 : 8);
        for (ssize_t off = 0; off < r;) {
            ssize_t w;
            if (k++ & 1) w = write(out, buf + off, r - off);
            else { struct iovec v[2] = { { buf + off, (r - off) / 2 }, { buf + off + (r - off) / 2, r - off - (r - off) / 2 } }; w = writev(out, v, 2); }
            if (w < 0) _exit(errno == EAGAIN ? 3 : 6);
            off += w;
        }
        left -= r;
    }
    _exit(0);
}
static int round_(long n, int heads) {
    int a[2], b[2];
    if (pipe(a) || pipe(b)) return 10;
    pid_t pp = fork();
    if (!pp) { close(a[0]); close(b[0]); close(b[1]); producer(a[1]); }
    pid_t hp[2];
    for (int h = 0; h < heads; h++) {
        hp[h] = fork();
        if (!hp[h]) { close(a[1]); close(b[0]); head(a[0], b[1], n / heads); }
    }
    close(a[0]); close(a[1]); close(b[1]);
    long total = 0; ssize_t r;
    while ((r = read(b[0], buf, sizeof buf)) > 0) total += r;
    close(b[0]);
    int bad = 0, st;
    for (int h = 0; h < heads; h++) { waitpid(hp[h], &st, 0); if (!WIFEXITED(st) || WEXITSTATUS(st)) { printf("head %d of %d: status %#x%s\n", h, heads, st, WIFEXITED(st) && WEXITSTATUS(st) == 3 ? " (EAGAIN on a blocking write)" : WIFEXITED(st) && WEXITSTATUS(st) == 7 ? " (EAGAIN on a blocking read)" : ""); bad = 1; } }
    kill(pp, SIGTERM); waitpid(pp, &st, 0);
    if (total != n) { printf("counted %ld of %ld (%d heads)\n", total, n, heads); bad = 1; }
    return bad;
}
int main(int argc, char **argv) {
    int rounds = atoi(argv[1]); long n = atol(argv[2]) << 20;
    signal(SIGPIPE, SIG_IGN);
    int bad = 0;
    for (int i = 0; i < rounds; i++) bad += round_(n, 1) + round_(n, 2);
    printf("%s %d rounds\n", bad ? "FAIL" : "ok", rounds);
    return !!bad;
}
EOF
zig cc -target "$TARGET" -O2 -static -Wl,--image-base=0x1000000 -o "$WORK/pe" "$WORK/pe.c"
"$GEXC" seed -o "$WORK/seed.wasm" >/dev/null 2>&1
GEX_SHARED=1 timeout "${TMO:-1200}" node "$ROOT/shims/browser/js/run-node.mjs" "$WORK/seed.wasm" "$WORK/pe" "$ROUNDS" "$MIB" > "$WORK/out" 2> "$WORK/err" || true
sed 's/\[gex[^]]*\] .*//; s/^\[.*//' "$WORK/out" | grep -v '^$' > "$WORK/guest" || true
if grep -q "^ok $ROUNDS rounds$" "$WORK/guest"; then
    echo "ok: $ROUNDS rounds of yes | head -c ${MIB}M | wc -c, one and two heads ($ARCH), no EAGAIN, every count exact"
    exit 0
fi
echo "FAIL: blocking pipe writes ($ARCH)" >&2
head -20 "$WORK/guest" >&2
tail -5 "$WORK/err" >&2
exit 1
