#!/bin/sh
# fork while another thread writes: the child's memory is the parent's at
# one moment, as Linux copies it. A writer thread fills a 4 MiB array with
# one value after another, bracketed by a sequence word (odd while it
# writes). Whatever moment a child gets, the array is all one value when
# the word is even, and one value then the one before it when it is odd.
# gex copied the address space page by page while the writer went on, so a
# child saw the word of one moment and the array of later ones; glibc's
# list of thread stacks, torn the same way, became a cycle that a forked
# child walked for ever (a KDE session's QProcess children).
#
#   tests/fork-copy.sh [a64|x86]      (needs zig and the guest's gexc)
set -eu
ARCH="${1:-a64}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${TMPDIR:-/tmp}/gex-fork-copy-$ARCH-$$"
case "$ARCH" in
a64) TARGET=aarch64-linux-musl; GEXC="${GEXC:-$ROOT/target/release/gexc}" ;;
x86) TARGET=x86_64-linux-musl; GEXC="${GEXC:-$ROOT/target/x86/release/gexc}"; export GEX_ARCH=x86 ;;
*) echo "usage: $0 [a64|x86]" >&2; exit 2 ;;
esac
mkdir -p "$WORK"
trap 'rm -rf "$WORK"' EXIT
cat > "$WORK/fc.c" <<'EOF2'
#include <pthread.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>
#define N (1u << 20)
static unsigned seq;
static unsigned arr[N];
static void *writer(void *arg) {
    for (unsigned v = 1;; v++) {
        __atomic_store_n(&seq, 2 * v - 1, __ATOMIC_SEQ_CST);
        for (unsigned i = 0; i < N; i++) __atomic_store_n(&arr[i], v, __ATOMIC_RELAXED);
        __atomic_store_n(&seq, 2 * v, __ATOMIC_SEQ_CST);
    }
    return arg;
}
/* in the child: whether the array fits the sequence word */
static int consistent(void) {
    unsigned s = seq, v = (s + 1) / 2, i = 0;
    if (s % 2 == 1) while (i < N && arr[i] == v) i++;
    unsigned rest = s % 2 == 1 ? v - 1 : v;
    for (; i < N; i++) if (arr[i] != rest) return 0;
    return 1;
}
int main(void) {
    pthread_t t;
    if (pthread_create(&t, 0, writer, 0) != 0) return 2;
    while (__atomic_load_n(&seq, __ATOMIC_SEQ_CST) < 4) ;
    int torn = 0;
    for (int k = 0; k < 40; k++) {
        pid_t p = fork();
        if (p == 0) _exit(consistent() ? 0 : 1);
        int st;
        if (p < 0 || waitpid(p, &st, 0) != p || !WIFEXITED(st)) return 3;
        torn += WEXITSTATUS(st) != 0;
    }
    printf("torn=%d\n", torn);
    fflush(stdout);
    _exit(torn != 0);
}
EOF2
zig cc -target "$TARGET" -O2 -static -Wl,--image-base=0x1000000 -o "$WORK/fc" "$WORK/fc.c"
got=$(timeout "${TMO:-300}" "$GEXC" run "$WORK/fc" 2>&1 | grep '^torn=' || true)
if [ "$got" = "torn=0" ]; then
    echo "ok fork-copy $ARCH: 40 children, each saw one moment"
else
    echo "FAIL fork-copy $ARCH: '${got:-no output}' (children that saw the writer's array torn)"
    exit 1
fi
