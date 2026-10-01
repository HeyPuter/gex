#!/bin/sh
# Output ordering across processes under the browser runtime's ProcessHost.
#
# A child that writes, flushes and exits, and a parent that waits for it and
# then writes: the child's line must come first. Each process is its own
# Worker and its output reaches the host as that Worker's messages, which
# nothing orders against another Worker's, so the host has to be told a
# process's output is delivered before its exit can be seen (`drain_output`,
# shims/browser). Stressed: several copies at once, each forking many times,
# on a loaded machine, which is when the misorder showed (S1: 7 of 16 runs).
#
#   tests/browser-output-order.sh [a64|x86] [copies] [forks]
#
# Needs zig (to build the guest), node, the guest's gexc, and the shared shim
# and kernel `shims/browser/build.sh` installs in web/public/gex.
set -eu
ARCH="${1:-a64}"
COPIES="${2:-16}"
FORKS="${3:-20}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${TMPDIR:-/tmp}/gex-output-order-$ARCH-$$"
case "$ARCH" in
a64) TARGET=aarch64-linux-musl; GEXC="${GEXC:-$ROOT/target/release/gexc}"; INFIX="" ;;
x86) TARGET=x86_64-linux-musl; GEXC="${GEXC:-$ROOT/target/x86/release/gexc}"; INFIX=".x86"; export GEX_ARCH=x86 ;;
*) echo "usage: $0 [a64|x86] [copies] [forks]" >&2; exit 2 ;;
esac
for need in "$GEXC" "$ROOT/web/public/gex/gex_browser$INFIX.shared.wasm" "$ROOT/web/public/gex/gex_kernel$INFIX.wasm"; do
    if [ ! -e "$need" ]; then
        echo "skip: $need is not built" >&2
        exit 0
    fi
done
command -v zig >/dev/null || { echo "skip: no zig to build the guest" >&2; exit 0; }
mkdir -p "$WORK"
[ -n "${KEEP:-}" ] || trap 'rm -rf "$WORK"' EXIT

cat > "$WORK/order.c" <<'EOF'
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <sys/wait.h>
int main(int argc, char **argv) {
    int n = argc > 1 ? atoi(argv[1]) : 10;
    for (int i = 0; i < n; i++) {
        pid_t p = fork();
        if (p == 0) {
            printf("child %d\n", i);
            fflush(stdout);
            _exit(0);
        }
        int st;
        waitpid(p, &st, 0);
        printf("parent %d\n", i);
        fflush(stdout);
    }
    return 0;
}
EOF
zig cc -target "$TARGET" -O2 -static -Wl,--image-base=0x1000000 -o "$WORK/order" "$WORK/order.c"
"$GEXC" seed -o "$WORK/seed.wasm" >/dev/null 2>&1

i=0
want="$WORK/want"
: > "$want"
while [ "$i" -lt "$FORKS" ]; do
    printf 'child %d\nparent %d\n' "$i" "$i" >> "$want"
    i=$((i + 1))
done

i=0
while [ "$i" -lt "$COPIES" ]; do
    GEX_SHARED=1 node "$ROOT/shims/browser/js/run-node.mjs" "$WORK/seed.wasm" "$WORK/order" "$FORKS" \
        > "$WORK/out.$i" 2> "$WORK/err.$i" &
    i=$((i + 1))
done
wait

bad=0
i=0
while [ "$i" -lt "$COPIES" ]; do
    # The runtime's own diagnostics share stdout (`[host]`, `[gex:1] ... fork`)
    # and can land inside a guest line, being another thread's console; the
    # guest's lines are what is left without them.
    sed 's/\[gex[^]]*\] .*//; s/^\[.*//' "$WORK/out.$i" | grep -v '^$' > "$WORK/guest.$i" || true
    if ! cmp -s "$want" "$WORK/guest.$i"; then
        bad=$((bad + 1))
        if [ "$bad" -eq 1 ]; then
            echo "run $i:" >&2
            diff "$want" "$WORK/guest.$i" | head -10 >&2
            tail -5 "$WORK/err.$i" >&2
        fi
    fi
    i=$((i + 1))
done
if [ "$bad" -ne 0 ]; then
    echo "FAIL: $bad of $COPIES runs ($FORKS forks each, $ARCH) printed out of order" >&2
    exit 1
fi
echo "ok: $COPIES runs x $FORKS forks ($ARCH), every child's output before its parent's"
