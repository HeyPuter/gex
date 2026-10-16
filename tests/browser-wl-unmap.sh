#!/bin/sh
# A Wayland window's destroy reaches the page while its process waits
# (tests/x86-programs/wl-unmap-pump.c, X's B2): a raw client maps a toplevel,
# destroys it and blocks on a futex for 3 s with nothing else running. A
# process's compositor runs on its own scheduler turns, and the first turn of
# a wait made without the kernel lock (`park_free`) skipped the pump, so the
# destroy was published only when the wait ended (a closed menu stayed drawn
# for seconds and took the next click), and a lone client blocked on its first
# reply never got one. Under node with the headless compositor
# (GEX_WL_STUB, GEX_WL_STUB_LOG): the publish without the window must come
# within 1 s of the destroy, not at the wake.
#
#   tests/browser-wl-unmap.sh [x86] [sp]      (default both)
#
# Needs the page's x86 shared build (`GEX_ARCH=x86 shims/browser/build.sh`)
# and gcc. A missing piece is a skip, not a pass.
set -u
cd "$(dirname "$0")/.."
SRC=tests/x86-programs/wl-unmap-pump.c
WORK=${WORK:-$(mktemp -d)}
mkdir -p "$WORK"
fail=0
jspi=
node -e 'process.exit(typeof WebAssembly.Suspending === "function" ? 0 : 1)' 2>/dev/null || jspi=--experimental-wasm-jspi
seed=web/public/gex/seed.x86.shared.wasm
if [ ! -f "$seed" ] || ! command -v gcc >/dev/null; then
    echo "skip wl-unmap (page): no x86 shared seed or gcc"
    exit 0
fi
bin=$WORK/wl-unmap-pump
gcc -O1 -static -o "$bin" "$SRC" || exit 1

for flavour in ${*:-x86 sp}; do
    single=; [ "$flavour" = sp ] && single=1
    env GEX_WL_STUB=1 GEX_WL_STUB_LOG=1 GEX_ARCH=x86 GEX_SHARED=1 ${single:+GEX_SINGLE=1} timeout 60 node $jspi \
        --import 'data:text/javascript,if(process.env.GEX_SINGLE)delete globalThis.SharedArrayBuffer' \
        shims/browser/js/run-node.mjs "$seed" "$bin" </dev/null >"$WORK/$flavour.out" 2>&1
    st=$?
    # The destroy's time, the wake's, and the first publish after the destroy
    # with no window left (the two streams interleave in any order).
    verdict=$(awk '
        /^destroyed t=/ { sub(/^destroyed t=/, ""); d = $0 + 0 }
        /^woke t=/ { sub(/^woke t=/, ""); w = $0 + 0 }
        /\[wlstub\] publish t=/ {
            split($0, f, /t=| surfaces=| windows=/)
            if (f[4] + 0 == 0) gone[n++] = f[2] + 0
        }
        END {
            if (!d || !w) { print "no destroy or wake printed"; exit }
            first = 0
            for (i = 0; i < n; i++) if (gone[i] >= d && (!first || gone[i] < first)) first = gone[i]
            if (!first) { print "no publish without the window"; exit }
            printf "%s %.0f ms after the destroy (the wake %.0f ms)\n", (first - d < 1000 ? "ok" : "late"), first - d, w - d
        }' "$WORK/$flavour.out")
    case "$st $verdict" in
    "0 ok"*) echo "ok   wl-unmap ($flavour, page): ${verdict#ok }" ;;
    *)
        echo "FAIL wl-unmap ($flavour, page): exit $st, $verdict"
        grep -v 'dispatch table' "$WORK/$flavour.out" | tail -8
        fail=1
        ;;
    esac
done
exit $fail
