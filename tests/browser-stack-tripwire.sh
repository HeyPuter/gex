#!/bin/sh
# A Worker's shim stack that overran its bottom is said, never silent: with
# each Worker's stack guard broken at its start (GEX_TEST_BREAK_STACK_GUARD,
# as an overrun leaves it), its next syscall or dispatch miss must report the
# overrun, once. wasm has no guard page, so a stack running past its
# megabyte lands on whatever the arena put below it, and only this check
# notices.
#
#   tests/browser-stack-tripwire.sh
#
# Needs the page's x86 shared build (`GEX_ARCH=x86 GEX_SHARED=1
# shims/browser/build.sh`) and zig or gcc. A missing piece is a skip.
set -u
cd "$(dirname "$0")/.."
WORK=${WORK:-$(mktemp -d)}
mkdir -p "$WORK"
seed=web/public/gex/seed.x86.shared.wasm
if [ ! -f "$seed" ]; then
    echo "skip stack-tripwire (page): no x86 shared seed"
    exit 0
fi
bin=$WORK/stack-tripwire
printf '#include <stdio.h>\nint main(void) { puts("hello"); return 0; }\n' > "$WORK/hello.c"
if command -v zig >/dev/null; then
    zig cc -target x86_64-linux-musl -O1 -static -o "$bin" "$WORK/hello.c" || exit 1
elif command -v gcc >/dev/null; then
    gcc -O1 -static -o "$bin" "$WORK/hello.c" || exit 1
else
    echo "skip stack-tripwire (page): no zig or gcc"
    exit 0
fi
env GEX_TEST_BREAK_STACK_GUARD=1 GEX_ARCH=x86 GEX_SHARED=1 timeout 60 \
    node shims/browser/js/run-node.mjs "$seed" "$bin" </dev/null >"$WORK/out" 2>&1
st=$?
n=$(grep -a -c "shim stack overran its bottom" "$WORK/out")
if [ "$st" = 0 ] && [ "$n" = 1 ] && grep -q '^hello$' "$WORK/out"; then
    echo "ok   stack-tripwire (page): $(grep -a -o 'worker [0-9]*.s shim stack overran.*found at [a-z ]*' "$WORK/out")"
else
    echo "FAIL stack-tripwire (page): exit $st, $n overrun reports (want 1)"
    tail -8 "$WORK/out"
    exit 1
fi
