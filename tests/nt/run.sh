#!/bin/bash
# The NT layer's gate (docs/nt-layer.md, stream S12): Windows console
# programs run through Wine 11.15's PE DLLs on gex, with GEX_NT_STRICT=1, so
# any call the NT layer does not implement ends the run.
#
#   tests/nt/run.sh [path/to/gexc]      (default target/release/gexc)
#
# gexc must be built with --features x86. The Wine prefix must be staged
# (tools/nt-stage-wine.sh). Each case checks stdout and the exit code.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
gexc=${1:-$root/target/release/gexc}
# Absolute: each case runs in its program's directory.
case $gexc in /*) ;; *) gexc=$PWD/$gexc ;; esac
fx=$root/crates/gex-ld/tests/pe/bin
[ -x "$gexc" ] || { echo "no gexc at $gexc" >&2; exit 2; }
prefix=${GEX_WINE_PREFIX:-$HOME/.cache/gex/wine-11.15}
[ -f "$prefix/.wine-version" ] || { echo "no staged Wine prefix at $prefix; run tools/nt-stage-wine.sh" >&2; exit 2; }
export GEX_NT_STRICT=1

pass=0 fail=0 xfail=0
# check NAME EXPECTED_EXIT EXPECTED_STDOUT(regex, whole output) EXE [ARGS...]
check() {
    local name=$1 want_code=$2 want_out=$3; shift 3
    local out code err
    err=$(mktemp)
    # A CRT's text-mode stdout writes CRLF, as on Windows; compared without
    # the CRs.
    out=$(cd "$(dirname "$1")" && timeout 120 "$gexc" run "$@" 2>"$err"; echo "exit=$?")
    code=${out##*exit=}
    out=${out%$'\n'exit=*}
    out=${out%exit=*}
    out=${out//$'\r'/}
    if [ "$code" = "$want_code" ] && [[ "$out" =~ ^${want_out}$ ]]; then
        pass=$((pass + 1)); echo "ok   $name"
    else
        fail=$((fail + 1)); echo "FAIL $name: exit $code (want $want_code)"
        echo "  stdout: $(printf %q "$out")"; echo "  want:   $want_out"
        sed 's/^/  stderr: /' "$err" | head -20
    fi
    rm -f "$err"
}

# xfail REASON NAME ...: a case known to fail for REASON, run anyway so its
# first pass is noticed (XPASS), not counted as a failure.
xfail() {
    local reason=$1 before=$fail; shift
    check "$@" >/dev/null
    if [ "$fail" -gt "$before" ]; then
        fail=$before; xfail=$((xfail + 1)); echo "xfail $1 ($reason)"
    else
        echo "XPASS $1 ($reason): drop the xfail"
    fi
}

nl=$'\n'
allok="(ok [^$nl]*$nl)+0 failures"
for arch in x64 x86; do
    b=$here/bin/$arch
    # N1 (x64) and N3 (x86): console programs.
    check $arch/hello_args 42 "hello from a Windows program with 3 args${nl}arg 1: a${nl}arg 2: b c" \
        "$b/hello_args.exe" a "b c"
    # N2 (and N3 for x86): every line "ok ...", then "0 failures".
    check $arch/sync 0 "$allok" "$b/sync.exe"
    check $arch/apc 0 "$allok" "$b/apc.exe"
    check $arch/tls 0 "$allok" "$b/tls.exe"
    check $arch/cxx 0 "$allok" "$b/cxx.exe"
    # N4: USER without a display.
    check $arch/user_win 0 "$allok" "$b/user_win.exe"
    check $arch/gdi 0 "$allok" "$b/gdi.exe"
    check $arch/input 0 "$allok" "$b/input.exe"
    check $arch/filemap 0 "$allok" "$b/filemap.exe"
    # A window that paints and runs a message loop (tests/nt/browser-gui.mjs shows it).
    check $arch/window 0 "shown${nl}bye" "$b/window.exe" 0
    # Code written, rewritten and reallocated at run time (a JIT).
    check $arch/jitcode 0 "$allok" "$b/jitcode.exe"
    check $arch/exitproc 33 "worker exiting${nl}atexit ran" "$b/exitproc.exe"
    # Ends in the top-level exception filter, which exits with 77.
    check $arch/seh 77 "(ok [^$nl]*$nl)+top-level filter saw e00000ff${nl}0 failures" "$b/seh.exe"
    # Divide errors and hlt (the lifter's Stmt::Trap).
    check $arch/seh-traps 0 "$allok" "$b/seh.exe" traps
done
# gex-ld's PE fixtures.
check pe-x64/hello 0 "Hello from x64 PE!" "$fx/x64/hello.exe"
check pe-x64/msvc 9 "msvc" "$fx/x64/msvc.exe"
check pe-x64/highbase 0 "tiny" "$fx/x64/highbase.exe"
check pe-x64/uses 0 "229 [0-9A-F]{16} 11" "$fx/x64/uses.exe"
check pe-x64/threads 0 "" "$fx/x64/threads.exe"
check pe-x86/hello32 0 "Hello from x86 PE32!" "$fx/x86/hello32.exe"
check pe-x86/msvc 9 "msvc" "$fx/x86/msvc.exe"
# (x86/uses.exe is left out: it crashes at exit on native Wine 11.15 too.)

echo "$pass passed, $fail failed, $xfail expected failures"
[ "$fail" = 0 ]
