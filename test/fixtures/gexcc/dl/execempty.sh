#!/bin/sh
# Empty strings among an exec's arguments and environment reach the new
# program, in place: a fork's child and a posix_spawn child exec the program
# again with "" between other arguments and an empty variable between two
# others, and it prints what it was given. The browser runtime hands an exec
# that goes on in another Worker over as a list of strings, which ended at
# the first empty argument (perl's system("logger", "", ...) ran logger with
# no arguments, which then read its message from the terminal). Natively
# (GEXC) and in the browser runtime (run-node, GEX_ROOT's AArch64 shim).
#
#   test/fixtures/gexcc/dl/execempty.sh DIR   (gexcc on PATH; GEXC: the gexc to test;
#                                   GEX_ROOT: a gex checkout with
#                                   page/public/gex built; DIR: where gex can
#                                   see, not /dev/shm)
set -u
command -v gexcc > /dev/null || { echo "skip execempty: gexcc not on PATH (toolchain/gexcc/install.sh)"; exit 0; }
G=${GEXC:?GEXC}
ROOT=${GEX_ROOT:?set GEX_ROOT to a gex checkout}
SEED=$ROOT/page/public/gex/seed.shared.wasm
[ -f "$SEED" ] || { echo "skip execempty: no $SEED"; exit 0; }
W=$(mktemp -d "${1:?DIR}/execempty.XXXXXX")
trap 'rm -rf "$W"' EXIT
cat > "$W/e.c" <<'X'
#include <spawn.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>
extern char **environ;
int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "C")) {
        printf("%d:", argc);
        for (int i = 1; i < argc; i++) printf(" [%s]", argv[i]);
        int n = 0;
        for (char **e = environ; *e; e++) n++;
        printf(" env %d [%s] [%s] [%s]\n", n, n > 0 ? environ[0] : "-", n > 1 ? environ[1] : "-", n > 2 ? environ[2] : "-");
        return 0;
    }
    char *args[] = { "/t/e", "C", "", "--tag=x", "", "--", "msg", 0 };
    char *env[] = { "A=1", "", "B=2", 0 };
    pid_t p = fork();
    if (p == 0) { execve("/t/e", args, env); _exit(127); }
    int s;
    waitpid(p, &s, 0);
    if (posix_spawn(&p, "/t/e", 0, 0, args, env)) return 1;
    waitpid(p, &s, 0);
    return 0;
}
X
gexcc -o "$W/e" "$W/e.c" || { echo "FAIL execempty: build"; exit 1; }
want='7: [C] [] [--tag=x] [] [--] [msg] env 3 [A=1] [] [B=2]
7: [C] [] [--tag=x] [] [--] [msg] env 3 [A=1] [] [B=2]'
fail=0
mkdir -p "$W/root/t" && cp "$W/e" "$W/root/t/e"
out=$(timeout 120 "$G" run --sysroot "$W/root" "$W/root/t/e" 2>&1 | grep -v '^\[')
[ "$out" = "$want" ] && echo "ok execempty (native)" || { echo "FAIL execempty (native):"; printf '%s\n' "$out"; fail=1; }
out=$(GEX_SHARED=1 timeout 120 node "$ROOT/hosts/browser/js/run-node.mjs" --file /t/e="$W/e" --argv0 /t/e \
    "$SEED" "$W/e" 2>&1 | grep -v '^\[')
[ "$out" = "$want" ] && echo "ok execempty (browser runtime)" || { echo "FAIL execempty (browser runtime):"; printf '%s\n' "$out"; fail=1; }
exit $fail
