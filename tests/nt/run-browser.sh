#!/bin/bash
# The NT layer's gate in the browser runtime (docs/nt-layer.md §10.2): the
# same cases as tests/nt/run.sh, each run by shims/browser/js/run-node.mjs --
# the shim and glue a page loads, under Node -- with GEX_NT_STRICT=1.
#
#   tests/nt/run-browser.sh            (ONLY=<regex> picks cases, ARCHES="x64")
#
# Needs the x86 shared-memory shim and seed in web/public/gex
# (GEX_ARCH=x86 shims/browser/build.sh) and a staged Wine prefix
# (tools/nt-stage-wine.sh), bundled as /wine.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
cd "$root"
prefix=${GEX_WINE_PREFIX:-$HOME/.cache/gex/wine-11.15}
[ -f "$prefix/.wine-version" ] || { echo "no staged Wine prefix at $prefix; run tools/nt-stage-wine.sh" >&2; exit 2; }
for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
    [ -f web/public/gex/$f ] || { echo "no web/public/gex/$f; build with GEX_ARCH=x86 shims/browser/build.sh" >&2; exit 2; }
done
fx=$root/crates/gex-ld/tests/pe/bin
pass=0 fail=0
only=${ONLY:-}
check() {
    local name=$1 want_code=$2 want_out=$3; shift 4
    [ -n "$only" ] && [[ ! "$name" =~ $only ]] && return
    local exe=$1; shift
    local base=$(basename "$exe")
    local out code err
    err=$(mktemp)
    local dlls=()
    for d in "$(dirname "$exe")"/*.dll; do [ -f "$d" ] && dlls+=(--file "/t/$(basename "$d")=$d"); done
    out=$(GEX_ARCH=x86 GEX_SHARED=1 timeout ${TMO:-300} node shims/browser/js/run-node.mjs \
        --dir /wine=$prefix --file /t/$base=$exe "${dlls[@]}" --argv0 /t/$base --cwd /t --env GEX_NT_STRICT=1 \
        web/public/gex/seed.x86.shared.wasm "$exe" "$@" 2>"$err" | grep -v '^\[host\]'; echo "exit=${PIPESTATUS[0]}")
    code=${out##*exit=}
    out=${out%$'\n'exit=*}
    out=${out%exit=*}
    out=${out//$'\r'/}
    if [ "$code" = "$want_code" ] && [[ "$out" =~ ^${want_out}$ ]]; then
        pass=$((pass + 1)); echo "ok   $name"
    else
        fail=$((fail + 1)); echo "FAIL $name: exit $code (want $want_code)"
        echo "  stdout: $(printf %q "$out" | head -c 600)"; echo "  want:   $want_out"
        grep -v "^\[host\]" "$err" | sed 's/^/  stderr: /' | head -20
    fi
    rm -f "$err"
}
nl=$'\n'
allok="(ok [^$nl]*$nl)+0 failures"
for arch in ${ARCHES:-x64 x86}; do
    b=$root/tests/nt/bin/$arch
    check $arch/hello_args 42 "hello from a Windows program with 3 args${nl}arg 1: a${nl}arg 2: b c" $arch "$b/hello_args.exe" a "b c"
    check $arch/sync 0 "$allok" $arch "$b/sync.exe"
    # Locks under contention. x86 only: x64's SList is cmpxchg16b, which
    # gex still does in two halves (wasm has no 16-byte atomic).
    [ $arch = x86 ] && check $arch/mtx 0 "$allok" $arch "$b/mtx.exe"
    check $arch/lowmem 0 "$allok" $arch "$b/lowmem.exe"
    check $arch/apc 0 "$allok" $arch "$b/apc.exe"
    check $arch/tls 0 "$allok" $arch "$b/tls.exe"
    check $arch/cxx 0 "$allok" $arch "$b/cxx.exe"
    check $arch/user_win 0 "$allok" $arch "$b/user_win.exe"
    check $arch/gdi 0 "$allok" $arch "$b/gdi.exe"
    check $arch/input 0 "$allok" $arch "$b/input.exe"
    check $arch/filemap 0 "$allok" $arch "$b/filemap.exe"
    # A window that paints and runs a message loop (tests/nt/browser-gui.mjs shows it).
    check $arch/window 0 "shown${nl}bye" $arch "$b/window.exe" 0
    # Code written, rewritten and reallocated at run time (a JIT).
    check $arch/jitcode 0 "$allok" $arch "$b/jitcode.exe"
    # A function's prologue patched at run time, as Detours does.
    check $arch/detour 0 "$allok" $arch "$b/detour.exe"
    check $arch/exitproc 33 "worker exiting${nl}atexit ran" $arch "$b/exitproc.exe"
    check $arch/seh 77 "(ok [^$nl]*$nl)+top-level filter saw e00000ff${nl}0 failures" $arch "$b/seh.exe"
    check $arch/seh-traps 0 "$allok" $arch "$b/seh.exe" traps
done
if [ -z "${ARCHES:-}" ] || [[ "$ARCHES" =~ x64 ]]; then
check pe-x64/hello 0 "Hello from x64 PE!" x64 "$fx/x64/hello.exe"
check pe-x64/msvc 9 "msvc" x64 "$fx/x64/msvc.exe"
check pe-x64/highbase 0 "tiny" x64 "$fx/x64/highbase.exe"
check pe-x64/uses 0 "229 [0-9A-F]{16} 11" x64 "$fx/x64/uses.exe"
check pe-x64/threads 0 "" x64 "$fx/x64/threads.exe"
fi
if [ -z "${ARCHES:-}" ] || [[ "$ARCHES" =~ x86 ]]; then
check pe-x86/hello32 0 "Hello from x86 PE32!" x86 "$fx/x86/hello32.exe"
check pe-x86/msvc 9 "msvc" x86 "$fx/x86/msvc.exe"
fi
# The arena keeps out of memory grown behind its back -- std's `System`
# allocator (dlmalloc) grows the memory itself for thread-local destructor
# lists, and the heap once landed on top of it (docs/nt-layer.md §10.2).
if [ -z "$only" ] || [[ arena-selfcheck =~ $only ]]; then
    selfcheck=$(GEX_ARCH=x86 GEX_SHARED=1 timeout ${TMO:-300} node shims/browser/js/run-node.mjs \
        --dir /wine=$prefix --file /t/hello_args.exe=$root/tests/nt/bin/x86/hello_args.exe --argv0 /t/hello_args.exe \
        --cwd /t --env GEX_NT_ARENA_SELFTEST=1 web/public/gex/seed.x86.shared.wasm \
        "$root/tests/nt/bin/x86/hello_args.exe" 2>&1 | grep "arena self-check")
    if [[ "$selfcheck" == *"arena self-check: ok"* ]]; then
        pass=$((pass + 1)); echo "ok   arena-selfcheck"
    else
        fail=$((fail + 1)); echo "FAIL arena-selfcheck: ${selfcheck:-no answer}"
    fi
fi
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
