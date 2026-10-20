#!/bin/sh
# A process out of memory fails cleanly under the browser runtime.
#
# The engine refuses a grow (Windows commits a wasm memory as it grows, so a
# page with many large processes meets this; see docs/mem64.md for sizes):
# here the memory is capped instead (GEX_MEM_MAX_MIB), which fails a grow
# the same way. The guest allocates until malloc says no -- big blocks
# (mmap), small ones (brk), and both from four threads at once -- and must
# see NULL/ENOMEM and exit on its own: no trap, no out-of-bounds access, no
# hang (a timeout fails the case). And the kernel's memory, capped too
# (GEX_KERNEL_MAX_MIB): a file filled in its filesystem until a write fails,
# after which a new process must still run (no kernel lock left held).
#
#   tests/browser-oom.sh [a64|x86] [process cap MiB] [kernel cap MiB]
#
# The kernel cap must leave room for its 256 MiB of Worker stacks and the rootfs.
#
# Needs zig, node, the guest's gexc, and the shared shim and kernel in
# web/public/gex (shims/browser/build.sh).
set -eu
ARCH="${1:-x86}"
CAP="${2:-320}"
KCAP="${3:-768}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${TMPDIR:-/tmp}/gex-oom-$ARCH-$$"
case "$ARCH" in
a64) TARGET=aarch64-linux-musl; GEXC="${GEXC:-$ROOT/target/release/gexc}"; INFIX="" ;;
x86) TARGET=x86_64-linux-musl; GEXC="${GEXC:-$ROOT/target/x86/release/gexc}"; INFIX=".x86"; export GEX_ARCH=x86 ;;
*) echo "usage: $0 [a64|x86] [cap MiB]" >&2; exit 2 ;;
esac
for need in "$GEXC" "$ROOT/web/public/gex/gex_browser$INFIX.shared.wasm" "$ROOT/web/public/gex/gex_kernel$INFIX.wasm"; do
    [ -e "$need" ] || { echo "skip: $need is not built" >&2; exit 0; }
done
command -v zig >/dev/null || { echo "skip: no zig to build the guest" >&2; exit 0; }
mkdir -p "$WORK"
[ -n "${KEEP:-}" ] || trap 'rm -rf "$WORK"' EXIT

cat > "$WORK/oom.c" <<'EOF'
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/wait.h>
/* Fill a file in the kernel's filesystem until a write fails, then check the
 * kernel still serves a new process: the errno, or -1 when it never failed. */
static int tmpfs(long cap_mib) {
    static char blk[1 << 20];
    memset(blk, 0x5a, sizeof blk);
    int fd = open("/tmp/oom-fill", O_CREAT | O_TRUNC | O_WRONLY, 0600);
    if (fd < 0) { printf("FAIL tmpfs: open: %s\n", strerror(errno)); return -2; }
    long n = 0; int err = 0;
    for (;;) {
        if (write(fd, blk, sizeof blk) != (ssize_t)sizeof blk) { err = errno; break; }
        if (++n > cap_mib * 2) return -1;
    }
    close(fd);
    unlink("/tmp/oom-fill");
    pid_t c = fork();
    if (!c) { int f = open("/tmp/oom-after", O_CREAT | O_WRONLY, 0600); _exit(f < 0 || write(f, "x", 1) != 1); }
    int st;
    if (c < 0 || waitpid(c, &st, 0) != c || !WIFEXITED(st) || WEXITSTATUS(st)) { printf("FAIL tmpfs: a process after the failed write: %#x\n", c < 0 ? -1 : st); return -2; }
    printf("ok tmpfs %ld (%s)\n", n, strerror(err));
    return 0;
}
/* Allocate blocks of `size` and touch them until malloc fails: the MiB held. */
/* Kept and summed, so the compiler cannot drop the malloc and the memset. */
static char *volatile last;
static volatile long sum;
static long fill(size_t size, long cap_mib) {
    long n = 0;
    for (;;) {
        char *p = malloc(size);
        if (!p) break;
        memset(p, 0x5a, size);
        *(char **)p = last;
        last = p;
        sum += p[size - 1];
        n++;
        if ((long)((n * size) >> 20) > cap_mib * 2) return -1; /* never refused */
    }
    return (long)((n * size) >> 20);
}
static long cap;
static void *thread(void *arg) {
    return (void *)fill((size_t)arg, cap);
}
int main(int argc, char **argv) {
    cap = atol(argv[2]);
    long got = 0;
    if (!strcmp(argv[1], "tmpfs")) { int r = tmpfs(cap); if (r == -1) printf("FAIL tmpfs: write never failed\n"); return r != 0; }
    if (!strcmp(argv[1], "big")) got = fill(1 << 20, cap);
    else if (!strcmp(argv[1], "small")) got = fill(4000, cap);
    else if (!strcmp(argv[1], "threads")) {
        pthread_t t[4];
        for (int i = 0; i < 4; i++) pthread_create(&t[i], 0, thread, (void *)(size_t)(i & 1 ? 4000 : 1 << 20));
        for (int i = 0; i < 4; i++) { void *r; pthread_join(t[i], &r); if ((long)r < 0) got = -1; else if (got >= 0) got += (long)r; }
    }
    if (got < 0) { printf("FAIL %s: malloc never failed\n", argv[1]); return 1; }
    printf("ok %s %ld\n", argv[1], got);
    return 0;
}
EOF
zig cc -target "$TARGET" -O2 -static -Wl,--image-base=0x1000000 -o "$WORK/oom" "$WORK/oom.c"
"$GEXC" seed -o "$WORK/seed.wasm" >/dev/null 2>&1
bad=0
for mode in big small threads tmpfs; do
    rc=0
    cap=$CAP; [ $mode = tmpfs ] && cap=$KCAP
    GEX_SHARED=1 GEX_MEM_MAX_MIB="$CAP" GEX_KERNEL_MAX_MIB="$KCAP" timeout "${TMO:-300}" node "$ROOT/shims/browser/js/run-node.mjs" "$WORK/seed.wasm" "$WORK/oom" "$mode" "$cap" \
        > "$WORK/out-$mode" 2> "$WORK/err-$mode" || rc=$?
    if [ "$rc" = 0 ] && grep -q "^ok $mode " "$WORK/out-$mode"; then
        echo "ok   oom/$mode (cap $cap MiB): $(grep "^ok $mode " "$WORK/out-$mode" | cut -d' ' -f3-) MiB before it failed"
        continue
    fi
    bad=1
    case "$rc" in 124) why="hung (timeout)" ;; *) why="exit $rc" ;; esac
    echo "FAIL oom/$mode (cap $cap MiB): $why" >&2
    grep -v '^$' "$WORK/out-$mode" | head -5 >&2
    grep -aE 'out of|threw|RangeError|bounds|unreachable|panick' "$WORK/err-$mode" | head -8 >&2
done
exit $bad
