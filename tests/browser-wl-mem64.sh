#!/bin/sh
# A memory64 process's wl_shm pool above 4 GiB reaches the page's compositor
# whole (tests/x86-programs/mem64-wl.c): a raw client maps a toplevel from a
# pool at 0x140000000 and prints the pixels' sum; the headless compositor
# (GEX_WL_STUB, GEX_WL_STUB_SUM) sums what it reads at the surface's address.
# The address used to cross as 32 bits (gex-wayland `Buffers::locate`, the
# surface record's one word), so the window was never published -- or would
# have been drawn from the low alias, the shim's heap.
#
#   tests/browser-wl-mem64.sh
#
# Needs the page's x86 memory64 shared build
# (`GEX_ARCH=x86 GEX_MEM64=1 GEX_SHARED=1 shims/browser/build.sh`) and zig
# or gcc. A missing piece is a skip, not a pass.
set -u
cd "$(dirname "$0")/.."
SRC=tests/x86-programs/mem64-wl.c
WORK=${WORK:-$(mktemp -d)}
mkdir -p "$WORK"
seed=web/public/gex/seed.x86.m64.shared.wasm
if [ ! -f "$seed" ]; then
    echo "skip wl-mem64 (page): no x86 memory64 shared seed"
    exit 0
fi
bin=$WORK/mem64-wl
if command -v zig >/dev/null; then
    zig cc -target x86_64-linux-musl -O1 -static -o "$bin" "$SRC" || exit 1
elif command -v gcc >/dev/null; then
    gcc -O1 -static -o "$bin" "$SRC" || exit 1
else
    echo "skip wl-mem64 (page): no zig or gcc"
    exit 0
fi
env XDG_RUNTIME_DIR=/tmp GEX_WL_STUB=1 GEX_WL_STUB_SUM=1 GEX_ARCH=x86 GEX_SHARED=1 GEX_MEM64=1 timeout 60 \
    node shims/browser/js/run-node.mjs "$seed" "$bin" </dev/null >"$WORK/wl-mem64.out" 2>&1
st=$?
pool=$(sed -n 's/^pool at //p' "$WORK/wl-mem64.out" | head -1)
want=$(sed -n 's/^sum=//p' "$WORK/wl-mem64.out" | head -1)
# The last publish of the 64x64 window: the one after its pixels were in.
seen=$(grep -a -o '\[wlstub\] window 64x64 at .*' "$WORK/wl-mem64.out" | tail -1)
case "$pool" in 0x[1-9]????????*) high=1 ;; *) high= ;; esac
if [ "$st" = 0 ] && [ -n "$high" ] && [ -n "$want" ] && [ "$seen" = "[wlstub] window 64x64 at $pool sum=$want" ]; then
    echo "ok   wl-mem64 (page): the compositor read the pool at $pool, sum $want"
else
    echo "FAIL wl-mem64 (page): exit $st, pool ${pool:-?}$([ -n "$high" ] || echo ' (not above 4 GiB)'), client sum ${want:-?}, compositor: ${seen:-no window published}"
    grep -v 'dispatch table' "$WORK/wl-mem64.out" | tail -8
    exit 1
fi
