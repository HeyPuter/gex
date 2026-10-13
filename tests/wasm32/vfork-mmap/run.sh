#!/bin/sh
# A vfork child that calls mmap, in the browser runtime (run-node.mjs): it must
# end as a crash, its parent must go on, and the warning must name the call
# (shims/browser/src/vfork.rs `check`).
#
#   tests/wasm32/vfork-mmap/run.sh    (GEX_WASM_SYSROOT; GEX_ARCH=aarch64 for
#                                      that build's shim, as run-browser.sh)
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../.." && pwd)
: "${GEX_WASM_SYSROOT:?set GEX_WASM_SYSROOT (tools/wine-wasm/build-sysroot.sh)}"
ARCH=${GEX_ARCH:-x86}
case $ARCH in x86) INFIX=.x86 ;; *) INFIX= ;; esac
SEED=$ROOT/web/public/gex/seed$INFIX.shared.wasm
WORK=${WORK:-$(mktemp -d)}
"$ROOT/tools/wine-wasm/wcc" -O2 -o "$WORK/vfork-mmap" "$HERE/main.c" || exit 1
out=$(GEX_ARCH=$ARCH GEX_SHARED=1 timeout "${TMO:-120}" node "$ROOT/shims/browser/js/run-node.mjs" \
    --file "/t/vfork-mmap=$WORK/vfork-mmap" --cwd /t --argv0 ./vfork-mmap "$SEED" "$WORK/vfork-mmap" 2>&1)
fail=0
# A crashed process's status is 137 (the shell's 128 + SIGKILL) here, as
# for any process that crashes in the browser runtime.
echo "$out" | grep -q "^child killed by a signal\|^child exited 137" || { echo "FAIL vfork-mmap: the child was not ended"; fail=1; }
echo "$out" | grep -q "^parent goes on" || { echo "FAIL vfork-mmap: the parent did not go on"; fail=1; }
echo "$out" | grep -q "a vfork child called mmap" || { echo "FAIL vfork-mmap: no warning naming mmap"; fail=1; }
if [ $fail = 0 ]; then echo "ok   vfork-mmap ($ARCH): the child ended, the parent went on, the warning names mmap"
else printf '%s\n' "$out" | grep -v '^\[host\]\|^\[worker\] ' | tail -20; fi
exit $fail
