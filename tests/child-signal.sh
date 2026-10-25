#!/bin/sh
# A child process killed by a signal says so on gex's stderr, as a shell
# would: one no crash handler catches (a SIGPIPE, a SIGKILL, an abort with
# none installed) otherwise left only its absence. A child ended
# by SIGTERM, as a session ends its processes, says nothing.
#
#   tests/child-signal.sh [a64|x86]      (needs zig and the guest's gexc)
set -eu
ARCH="${1:-a64}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${TMPDIR:-/tmp}/gex-child-signal-$ARCH-$$"
case "$ARCH" in
a64) TARGET=aarch64-linux-musl; GEXC="${GEXC:-$ROOT/target/release/gexc}" ;;
x86) TARGET=x86_64-linux-musl; GEXC="${GEXC:-$ROOT/target/x86/release/gexc}"; export GEX_ARCH=x86 ;;
*) echo "usage: $0 [a64|x86]" >&2; exit 2 ;;
esac
[ -x "$GEXC" ] || { echo "skip: $GEXC is not built"; exit 0; }
command -v zig >/dev/null || { echo "skip: no zig to build the guest"; exit 0; }
mkdir -p "$WORK"
[ -n "${KEEP:-}" ] || trap 'rm -rf "$WORK"' EXIT
cat > "$WORK/cs.c" <<'EOF2'
#include <signal.h>
#include <stdlib.h>
#include <stdio.h>
#include <sys/wait.h>
#include <unistd.h>
static int child(int how) {
    pid_t p = fork();
    if (p == 0) {
        if (how == 0) { int fd[2]; pipe(fd); close(fd[0]); write(fd[1], "x", 1); }
        if (how == 1) abort();
        if (how == 3) kill(getpid(), SIGKILL);
        if (how == 2) raise(SIGTERM);
        _exit(0);
    }
    int st = 0;
    waitpid(p, &st, 0);
    printf("child %d: pid %d signal %d\n", how, p, WIFSIGNALED(st) ? WTERMSIG(st) : 0);
    fflush(stdout);
    return p;
}
int main(void) {
    for (int how = 0; how < 4; how++) child(how);
    return 0;
}
EOF2
zig cc -target "$TARGET" -O1 -static -Wl,--image-base=0x1000000 -o "$WORK/cs" "$WORK/cs.c"
timeout "${TMO:-120}" "$GEXC" run "$WORK/cs" > "$WORK/out" 2> "$WORK/err" || true
fail=0
for want in "0 13 yes" "1 6 yes" "2 15 no" "3 9 yes"; do
    set -- $want
    line=$(grep "^child $1:" "$WORK/out" || true)
    pid=$(echo "$line" | sed -n 's/.* pid \([0-9]*\) .*/\1/p')
    sig=$(echo "$line" | sed -n 's/.* signal \([0-9]*\)$/\1/p')
    said=no; [ -n "$pid" ] && grep -q "pid $pid killed by signal $2 " "$WORK/err" && said=yes
    if [ "$sig" = "$2" ] && [ "$said" = "$3" ]; then :; else
        echo "FAIL child-signal $ARCH: child $1 got signal '${sig:-?}' (want $2), said: $said (want $3)"
        fail=1
    fi
done
[ $fail = 0 ] && echo "ok child-signal $ARCH: SIGPIPE, SIGABRT and SIGKILL children named, SIGTERM quiet"
exit $fail
