#!/bin/sh
# W64's LP64 programs in the browser runtime (shims/browser/js/run-node.mjs:
# the shim and glue a page loads, under Node), as tests/wasm64/run.sh runs
# them under gexc: each built with tools/wine-wasm/wcc64 (wasm64 lowered to
# wasm32, ELF64 container) against its native run. Every program runs in the
# default (SharedArrayBuffer) build and in the single-thread one
# (GEX_SINGLE=1, the SP page's), which use the same binaries.
#
#   tests/wasm64/run-browser.sh   (GEX_WASM64_SYSROOT set; the x86 shared shim
#                                  and seed in web/public/gex:
#                                  GEX_ARCH=x86 GEX_SHARED=1 shims/browser/build.sh)
set -u
# Output is compared with the native run: not even gex's default lines (log.mjs).
export GEX_LOG=${GEX_LOG:-off}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
: "${GEX_WASM64_SYSROOT:?set GEX_WASM64_SYSROOT (tools/wine-wasm/build-sysroot64.sh)}"
SEED=$ROOT/web/public/gex/seed.x86.shared.wasm
[ -f "$SEED" ] || { echo "no $SEED; build with GEX_ARCH=x86 GEX_SHARED=1 shims/browser/build.sh" >&2; exit 2; }
WORK=${WORK:-$(mktemp -d)}
fail=0
# The single-thread build needs JSPI (on by default from node 25, behind a
# flag before) and runs with no SharedArrayBuffer, as the SP page does.
jspi=
node -e 'process.exit(typeof WebAssembly.Suspending === "function" ? 0 : 1)' 2>/dev/null || jspi=--experimental-wasm-jspi
node_run() { # single args...
    s=$1; shift
    env ${s:+GEX_SINGLE=1} GEX_ARCH=x86 GEX_SHARED=1 timeout "${TMO:-300}" node $jspi \
        --import 'data:text/javascript,if(process.env.GEX_SINGLE)delete globalThis.SharedArrayBuffer' \
        "$ROOT/shims/browser/js/run-node.mjs" "$@"
}
run() { # single args...
    node_run "$@" 2>"$WORK/err" | grep -v '^\[host\]\|^\[gex\] \|^\[worker\] '
}
for src in "$HERE/lp64.c" "$HERE/../wasm32/signals.c" "$HERE/../wasm32/threads.c" \
           "$HERE/../wasm32/dlopen.c" "$HERE/../wasm32/sjlj.c" "$HERE/../wasm32/inet.c"; do
    name=$(basename "$src" .c)
    cc -O1 -o "$WORK/$name.native" "$src" -lpthread || { fail=1; continue; }
    "$ROOT/tools/wine-wasm/wcc64" -O2 -o "$WORK/$name" "$src" || { echo "FAIL $name: wcc64"; fail=1; continue; }
    native=$(cd "$WORK" && "./$name.native" 2>/dev/null); ns=$?
    for flavour in mp sp; do
        s=; [ $flavour = sp ] && s=1
        web=$(run "$s" --file "/t/$name=$WORK/$name" --cwd /t --argv0 "./$name" "$SEED" "$WORK/$name")
        # (the exit status of `run`'s pipeline is grep's: take node's)
        ws=$(node_run "$s" --file "/t/$name=$WORK/$name" --cwd /t --argv0 "./$name" "$SEED" "$WORK/$name" >/dev/null 2>&1; echo $?)
        if [ "$native" = "$web" ] && [ "$ns" = "$ws" ]; then
            echo "ok   wasm64/$name (browser $flavour, exit $ns)"
        else
            echo "FAIL wasm64/$name (browser $flavour): native exit $ns, browser exit $ws"
            printf '%s\n' "$native" > "$WORK/$name.out-native"
            printf '%s\n' "$web" > "$WORK/$name.out-web-$flavour"
            diff "$WORK/$name.out-native" "$WORK/$name.out-web-$flavour" | head -20
            grep -v '^\[host\]\|^\[worker\] ' "$WORK/err" | head -5 | sed 's/^/  stderr: /'
            fail=1
        fi
    done
done
# The --gex-guest-x86-64 flag (header flag 2): Windows' address space.
want='anonymous mapping below the USD: 1
USD and the page after it free: 1'
if "$ROOT/tools/wine-wasm/wcc64" -O2 -Wl,--gex-guest-x86-64 -o "$WORK/winspace" "$HERE/winspace.c"; then
    for flavour in mp sp; do
        s=; [ $flavour = sp ] && s=1
        got=$(run "$s" --file "/t/winspace=$WORK/winspace" --cwd /t --argv0 ./winspace "$SEED" "$WORK/winspace")
        if [ "$got" = "$want" ]; then
            echo "ok   wasm64/winspace (browser $flavour, guest x86-64 flag)"
        else
            echo "FAIL wasm64/winspace (browser $flavour, guest x86-64 flag)"; printf '%s\n' "$got" | sed 's/^/  got: /'; fail=1
        fi
    done
else
    echo "FAIL winspace: wcc64"; fail=1
fi
# And past 2 GiB: a W64 process's space is the NT layer's split, not W3's
# compact one (which ends at 2 GiB: 1536 MiB here before).
want='mapped 2560 MiB, above 2 GiB 1'
if "$ROOT/tools/wine-wasm/wcc64" -O2 -Wl,--gex-guest-x86-64 -o "$WORK/winspace-high" "$HERE/winspace-high.c"; then
    for flavour in mp sp; do
        s=; [ $flavour = sp ] && s=1
        got=$(run "$s" --file "/t/winspace-high=$WORK/winspace-high" --cwd /t --argv0 ./winspace-high "$SEED" "$WORK/winspace-high")
        if [ "$got" = "$want" ]; then
            echo "ok   wasm64/winspace-high (browser $flavour, past 2 GiB)"
        else
            echo "FAIL wasm64/winspace-high (browser $flavour, past 2 GiB)"; printf '%s\n' "$got" | sed 's/^/  got: /'
            grep -a 'arena cannot grow' "$WORK/err" | head -2 | sed 's/^/  stderr: /'; fail=1
        fi
    done
else
    echo "FAIL winspace-high: wcc64"; fail=1
fi
exit $fail
