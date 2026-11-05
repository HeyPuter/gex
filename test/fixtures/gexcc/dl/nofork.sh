#!/bin/sh
# -fgex-no-fork (GEXCC_NOFORK=1): a link that never forks. Its fork, and a
# library's built the same way, fail with ENOSYS; posix_spawn still starts
# a child; and neither module carries asyncify. Without the flag the same
# sources fork, and asyncify is there. Static and shared (libc.so) links.
#
#   test/fixtures/gexcc/dl/nofork.sh DIR    (gexcc on PATH; DIR: where gex can see,
#                                 not /dev/shm)
set -u
command -v gexcc > /dev/null || { echo "gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 2; }
W=$(mktemp -d "${1:?DIR}/nofork.XXXXXX")
trap 'rm -rf "$W"' EXIT
cat > "$W/p.c" <<'X'
#include <errno.h>
#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>
extern char **environ;
#ifdef LIB
int lib_fork(void);
#endif
int main(int argc, char **argv) {
    if (argc > 1) return 7;
    pid_t p = fork();
    if (p == 0) _exit(3);
    if (p > 0) { int st; waitpid(p, &st, 0); printf("fork %d\n", WEXITSTATUS(st)); }
    else printf("fork %s\n", errno == ENOSYS ? "ENOSYS" : strerror(errno));
#ifdef LIB
    printf("lib %s\n", lib_fork() == 0 ? "forked" : errno == ENOSYS ? "ENOSYS" : strerror(errno));
#endif
    char *av[] = {argv[0], "child", 0};
    pid_t c; int st;
    int r = posix_spawn(&c, argv[0], 0, 0, av, environ);
    if (r) { printf("spawn %s\n", strerror(r)); return 1; }
    waitpid(c, &st, 0);
    printf("spawn %d\n", WEXITSTATUS(st));
    return 0;
}
X
cat > "$W/l.c" <<'X'
#include <sys/wait.h>
#include <unistd.h>
int lib_fork(void) { pid_t p = fork(); if (p == 0) _exit(0); if (p > 0) waitpid(p, 0, 0); return p < 0 ? -1 : 0; }
X
fail=0
# asyncify's exports, in a program's kept wasm or a side module
asy() { grep -a -c asyncify_start_unwind "$1"; }
check() {  # NAME WANT GOT
    if [ "$2" = "$3" ]; then echo "ok   nofork-$1"; else echo "FAIL nofork-$1: '$3' (want '$2')"; fail=1; fi
}
cd "$W" || exit 2
# static
GEXCC_KEEP_WASM=1 gexcc -O2 -o fs p.c && GEXCC_KEEP_WASM=1 gexcc -O2 -fgex-no-fork -o ns p.c || { echo "FAIL nofork: static build"; exit 1; }
check static-fork "fork 3 spawn 7 1" "$(./fs 2>/dev/null | tr '\n' ' ')$(asy fs.wasm)"
check static-nofork "fork ENOSYS spawn 7 0" "$(./ns 2>/dev/null | tr '\n' ' ')$(asy ns.wasm)"
# shared: the environment variable, and a library built with the flag
if [ -f "$(gexcc --print-sysroot)/lib/libc.so" ]; then
    export LD_LIBRARY_PATH="$(gexcc --print-sysroot)/lib:$W"
    GEXCC_SHARED=1 gexcc -O2 -fPIC -shared -o libfl.so l.c &&
        GEXCC_SHARED=1 GEXCC_NOFORK=1 gexcc -O2 -fPIC -shared -o libnl.so l.c &&
        GEXCC_SHARED=1 GEXCC_KEEP_WASM=1 gexcc -O2 -DLIB -o fd p.c -L. -lfl &&
        GEXCC_SHARED=1 GEXCC_KEEP_WASM=1 GEXCC_NOFORK=1 gexcc -O2 -DLIB -o nd p.c -L. -lnl ||
        { echo "FAIL nofork: shared build"; exit 1; }
    check shared-fork "fork 3 lib forked spawn 7 1 1" "$(./fd 2>/dev/null | tr '\n' ' ')$(asy fd.wasm) $(asy libfl.so)"
    check shared-nofork "fork ENOSYS lib ENOSYS spawn 7 0 0" "$(./nd 2>/dev/null | tr '\n' ' ')$(asy nd.wasm) $(asy libnl.so)"
else
    echo "ok   nofork-shared: no libc.so in this gexcc"
fi
[ $fail = 0 ] && echo "ok nofork" || { echo "FAIL nofork"; exit 1; }
