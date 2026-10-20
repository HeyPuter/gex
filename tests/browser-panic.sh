#!/bin/sh
# A panic in the browser shim always reaches the console: a plain one, one
# that panics again inside the panic hook (std drops that one's message; the
# first must stay, and the trap's report must say a second one happened),
# and one with a 4 KiB message (its first 1 KiB also as a line of its own).
# The shim panics on purpose through GEX_DEBUG_PANIC (gex_debug_panic).
#
#   tests/browser-panic.sh
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${TMPDIR:-/tmp}/gex-panic-$$"
GEXC="${GEXC:-$ROOT/target/x86/release/gexc}"
export GEX_ARCH=x86
for need in "$GEXC" "$ROOT/web/public/gex/gex_browser.x86.shared.wasm" "$ROOT/web/public/gex/gex_kernel.x86.wasm"; do
    [ -e "$need" ] || { echo "skip: $need is not built" >&2; exit 0; }
done
command -v zig >/dev/null || { echo "skip: no zig to build the guest" >&2; exit 0; }
mkdir -p "$WORK"
[ -n "${KEEP:-}" ] || trap 'rm -rf "$WORK"' EXIT
printf 'int main(void) { return 0; }\n' > "$WORK/t.c"
zig cc -target x86_64-linux-musl -O2 -static -o "$WORK/t" "$WORK/t.c"
"$GEXC" seed -o "$WORK/seed.wasm" >/dev/null 2>&1
fail=0
check() { # mode, then fixed strings the output must hold
    mode=$1; shift
    env GEX_SHARED=1 GEX_DEBUG_PANIC="$mode" timeout "${TMO:-120}" node "$ROOT/shims/browser/js/run-node.mjs" \
        "$WORK/seed.wasm" "$WORK/t" > "$WORK/$mode.out" 2>&1 || true
    for want in "$@"; do
        if ! grep -qF -- "$want" "$WORK/$mode.out"; then
            echo "FAIL panic $mode: no \"$want\" in the output ($WORK/$mode.out)"
            fail=1
            return
        fi
    done
    echo "ok   panic $mode"
}
check plain "gex: panic in the shim: panicked at" "gex debug panic: plain"
check nested "gex debug panic: outer" "it panicked again inside the panic report"
# The head line: the first 1 KiB, then the length; the whole line after it.
check long "bytes; the whole message follows)" "gex debug panic: long xxxx"
if [ "$fail" = 0 ]; then
    n=$(grep -c "gex debug panic: long" "$WORK/long.out" || true)
    [ "$n" -ge 2 ] || { echo "FAIL panic long: the message came $n times, wanted the head and the whole"; fail=1; }
fi
exit $fail
