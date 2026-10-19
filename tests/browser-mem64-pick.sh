#!/bin/sh
# The memory flavour chosen per program, at exec (docs/mem64.md §5), in the
# default page, whose processes have the lowered wasm32 memory: an exec with
# GEX_MEM64=1 in its environment, or of an image whose user.gex.mem64
# attribute is 1 (the learned record), goes on in a 64-bit memory. The flag
# applies to that exec alone: the kernel takes it out of the new program's
# environment, so the 64-bit program's own children -- by fork and exec, and
# by posix_spawn -- are 32-bit again, and so is a sibling exec'd without it.
# GEX_MEM64=0 overrides the record.
#
# A process tells which memory it has by a mapping asked for at 5 GiB: only
# a 64-bit memory can give it there.
#
#   tests/browser-mem64-pick.sh
#
# Needs zig, node, the x86 gexc, and in web/public/gex the x86 shared shim
# and kernel and the 64-bit memory's shim and seed (shims/kernel/build.sh,
# shims/browser/build.sh, each with GEX_ARCH=x86 GEX_SHARED=1, and the
# GEX_MEM64=1 build).
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${TMPDIR:-/tmp}/gex-mem64-pick-$$"
GEXC="${GEXC:-$ROOT/target/x86/release/gexc}"
export GEX_ARCH=x86
P="$ROOT/web/public/gex"
for need in "$GEXC" "$P/gex_browser.x86.shared.wasm" "$P/gex_kernel.x86.wasm" "$P/gex_browser.x86.m64.shared.wasm" "$P/seed.x86.m64.shared.wasm"; do
    [ -e "$need" ] || { echo "skip: $need is not built" >&2; exit 0; }
done
command -v zig >/dev/null || { echo "skip: no zig to build the guest" >&2; exit 0; }
mkdir -p "$WORK"
[ -n "${KEEP:-}" ] || trap 'rm -rf "$WORK"' EXIT

cat > "$WORK/pick.c" <<'EOF2'
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <unistd.h>
extern char **environ;
#define ME "/g/pick"

static const char *flavour(void) {
    void *p = mmap((void *)0x140000000ull, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    return p != MAP_FAILED && (uintptr_t)p >= (1ull << 32) ? "64" : "32";
}

/* Fork and exec `who NAME [kids]`, with `env` (NULL: this one's). */
static void run(const char *name, const char *kids, char **env) {
    pid_t p = fork();
    if (p == 0) {
        char *av[] = { ME, "who", (char *)name, (char *)kids, 0 };
        execve(ME, av, env ? env : environ);
        _exit(127);
    }
    waitpid(p, 0, 0);
}

static void spawn(const char *name, char **env) {
    pid_t p;
    char *av[] = { ME, "who", (char *)name, 0 };
    if (posix_spawn(&p, ME, 0, 0, av, env ? env : environ) == 0) waitpid(p, 0, 0);
    else printf("%s: posix_spawn failed\n", name);
}

int main(int argc, char **argv) {
    static char *ask64[] = { "GEX_MEM64=1", "HOME=/", 0 };
    static char *ask32[] = { "GEX_MEM64=0", "HOME=/", 0 };
    setvbuf(stdout, 0, _IONBF, 0);
    if (argc >= 3 && !strcmp(argv[1], "who")) {
        printf("%s: %s%s\n", argv[2], flavour(), getenv("GEX_MEM64") ? " (GEX_MEM64 leaked)" : "");
        if (argc >= 4 && argv[3] && !strcmp(argv[3], "kids")) {
            run("its child", 0, 0);
            spawn("its spawned child", 0);
        }
        return 0;
    }
    printf("parent: %s\n", flavour());
    run("asked", "kids", ask64);
    run("sibling", 0, 0);
    spawn("spawned, asked", ask64);
    if (setxattr(ME, "user.gex.mem64", "1", 1, 0)) { printf("setxattr failed\n"); return 1; }
    run("recorded", 0, 0);
    spawn("spawned, recorded", 0);
    run("recorded, asked 32", 0, ask32);
    removexattr(ME, "user.gex.mem64");
    run("record gone", 0, 0);
    return 0;
}
EOF2
zig cc -target x86_64-linux-musl -O2 -static -o "$WORK/pick" "$WORK/pick.c"
"$GEXC" seed -o "$WORK/seed.wasm" >/dev/null 2>&1
want="parent: 32
asked: 64
its child: 32
its spawned child: 32
sibling: 32
spawned, asked: 64
recorded: 64
spawned, recorded: 64
recorded, asked 32: 32
record gone: 32"
GEX_SHARED=1 timeout "${TMO:-180}" node "$ROOT/shims/browser/js/run-node.mjs" --file /g/pick="$WORK/pick" --argv0 /g/pick "$WORK/seed.wasm" "$WORK/pick" > "$WORK/out" 2> "$WORK/err" || true
got=$(grep -v '^\[' "$WORK/out" || true)
if [ "$got" = "$want" ]; then
    echo "ok: the memory flavour is chosen per program at exec (GEX_MEM64 and user.gex.mem64), and the 64-bit program's children and siblings stay 32-bit"
    exit 0
fi
echo "FAIL: the per-program memory flavour: got" >&2
echo "$got" >&2
tail -8 "$WORK/err" >&2
exit 1
