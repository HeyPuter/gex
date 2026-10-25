#!/bin/sh
# A child process killed by a crash signal says so on gex's stderr, as a
# shell would: one no crash handler catches (an abort with none installed, a
# SIGSEGV, a SIGKILL from another process) otherwise left only its absence.
# A pipeline's SIGPIPE and a session's SIGTERM say nothing, unless
# GEX_LOG_CHILD_SIGNALS=1 asks for every signal.
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
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>
int main(void) {
    for (int how = 0; how < 4; how++) {
        pid_t p = fork();
        if (p == 0) {
            if (how == 0) { int fd[2]; pipe(fd); close(fd[0]); write(fd[1], "x", 1); }
            if (how == 1) abort();
            if (how == 2) raise(SIGTERM);
            if (how == 3) raise(SIGSEGV);
            _exit(0);
        }
        if (p > 0 && how == 3) {
            /* and one another process kills */
            pid_t q = fork();
            if (q == 0) { for (;;) pause(); }
            kill(q, SIGKILL);
            int st = 0;
            waitpid(q, &st, 0);
            printf("child 4: pid %d signal %d\n", q, WIFSIGNALED(st) ? WTERMSIG(st) : 0);
        }
        int st = 0;
        waitpid(p, &st, 0);
        printf("child %d: pid %d signal %d\n", how, p, WIFSIGNALED(st) ? WTERMSIG(st) : 0);
        fflush(stdout);
    }
    return 0;
}
EOF2
zig cc -target "$TARGET" -O1 -static -Wl,--image-base=0x1000000 -o "$WORK/cs" "$WORK/cs.c"
fail=0
# Each case: child, its signal, whether it is said by default, and with every signal asked for.
for mode in default all; do
    if [ $mode = all ]; then export GEX_LOG_CHILD_SIGNALS=1; else unset GEX_LOG_CHILD_SIGNALS; fi
    timeout "${TMO:-120}" "$GEXC" run "$WORK/cs" > "$WORK/out.$mode" 2> "$WORK/err.$mode" || true
    for want in "0 13 no yes" "1 6 yes yes" "2 15 no yes" "3 11 yes yes" "4 9 yes yes"; do
        set -- $want
        expect=$3; [ $mode = all ] && expect=$4
        line=$(grep "^child $1:" "$WORK/out.$mode" || true)
        pid=$(echo "$line" | sed -n 's/.* pid \([0-9]*\) .*/\1/p')
        sig=$(echo "$line" | sed -n 's/.* signal \([0-9]*\)$/\1/p')
        said=no; [ -n "$pid" ] && grep -q "pid $pid killed by signal $2 " "$WORK/err.$mode" && said=yes
        if [ "$sig" != "$2" ] || [ "$said" != "$expect" ]; then
            echo "FAIL child-signal $ARCH ($mode): child $1 got signal '${sig:-?}' (want $2), said: $said (want $expect)"
            fail=1
        fi
    done
done
[ $fail = 0 ] && echo "ok child-signal $ARCH: SIGABRT, SIGSEGV and SIGKILL children named, SIGPIPE and SIGTERM only when asked"
exit $fail
