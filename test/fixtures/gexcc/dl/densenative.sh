#!/bin/sh
# A native wasm32 program numbers its blocks densely, and so does its fork
# child. It numbers pages too (for the units its libraries call through), and
# a classic table reaches the highest number any process gave, so a classic
# one costs every Worker of a busy system its whole width. In the browser
# runtime (run-node, GEX_LOG=tables): each process must grow `blocks` and
# no dispatch table 1-7.
#
#   test/fixtures/gexcc/dl/densenative.sh DIR   (gexcc on PATH; GEX_ROOT: a gex checkout
#                                     with page/public/gex built for x86, or
#                                     GEX_WEB_DIR; DIR: where gex can see,
#                                     not /dev/shm)
set -u
command -v gexcc > /dev/null || { echo "skip densenative: gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 0; }
ROOT=${GEX_ROOT:?set GEX_ROOT to a gex checkout}
WEB=${GEX_WEB_DIR:-$ROOT/page/public/gex}
SEED=$WEB/seed.x86.shared.wasm
[ -f "$SEED" ] || { echo "skip densenative: no $SEED"; exit 0; }
W=$(mktemp -d "${1:?DIR}/densenative.XXXXXX")
trap 'rm -rf "$W"' EXIT
cat > "$W/n.c" <<'X'
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>
static int cmp(const void *a, const void *b) { return *(const int *)a - *(const int *)b; }
static int sorted(int k) {
    int v[64];
    for (int i = 0; i < 64; i++) v[i] = (i * 37 + k) % 64;
    qsort(v, 64, sizeof v[0], cmp);
    return v[63];
}
int main(void) {
    printf("parent %d\n", sorted(1));
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) { printf("child %d %s\n", sorted(2), strerror(2)); return 0; }
    int st = 0;
    waitpid(p, &st, 0);
    printf("waited %d\n", WEXITSTATUS(st));
    return 0;
}
X
gexcc -O1 -o "$W/n" "$W/n.c" || { echo "FAIL densenative: build"; exit 1; }
out=$(GEX_WEB_DIR=$WEB GEX_ARCH=x86 GEX_SHARED=1 GEX_LOG=tables timeout 120 \
    node "$ROOT/hosts/browser/js/run-node.mjs" --file /t/n="$W/n" --argv0 /t/n "$SEED" "$W/n" 2>&1)
fail=0
for l in "parent 63" "child 63 No such file or directory" "waited 0"; do
    printf '%s\n' "$out" | grep -qx "$l" || { echo "FAIL densenative: no line [$l]"; fail=1; }
done
for pid in $(printf '%s\n' "$out" | sed -n 's/.*\[tables\] pid \([0-9]*\) .*/\1/p' | sort -u); do
    blocks=$(printf '%s\n' "$out" | grep -c "\[tables\] pid $pid .*blocks grew" || true)
    classic=$(printf '%s\n' "$out" | grep -c "\[tables\] pid $pid .*dispatch[1-7] grew" || true)
    [ "$blocks" -gt 0 ] && [ "$classic" = 0 ] || { echo "FAIL densenative: pid $pid: blocks grew $blocks, classic tables grew $classic"; fail=1; }
done
n=$(printf '%s\n' "$out" | sed -n 's/.*\[tables\] pid \([0-9]*\) .*/\1/p' | sort -u | wc -l)
[ "$n" -ge 2 ] || { echo "FAIL densenative: tables from $n processes, want parent and child"; fail=1; }
[ $fail -eq 0 ] && echo "ok densenative" || printf '%s\n' "$out" | tail -20
exit $fail
