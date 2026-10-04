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
# GEXC_RUN_FLAGS: flags for every `gexc run` (the gate runs the suite with
# --no-cgen and with --cgen). Units go to a cache of the checkout's own unless
# GEX_CGEN_CACHE says otherwise, so a test run never fills the user's; under
# --cgen a test program without one gets it built before it runs.
flags=${GEXC_RUN_FLAGS:-}
export GEX_CGEN_CACHE=${GEX_CGEN_CACHE:-$root/target/nt-test-cgen}
case " $flags " in *" --cgen "*) export GEX_NT_CGEN_SYNC=1 ;; esac

# icall.exe's indirect-call profile, as tools/icall-hints.py writes one
# (x86: its three call sites and their dominant targets), so under --cgen its
# unit has the guarded direct calls and the fallback through the table runs.
case " $flags " in *" --cgen "*)
    hints=$("$gexc" cgen-c "$here/bin/x86/icall.exe" --pe-base 0x400000 --icall-path) &&
        mkdir -p "$(dirname "$hints")" &&
        printf 'icall 0x402a9b 0x401560 1000\nicall 0x402aae 0x401590 1000\nicall 0x402ae7 0x401560 980\nicall 0x402ae7 0x401570 20\n' > "$hints" ;;
esac

pass=0 fail=0 xfail=0
# check NAME EXPECTED_EXIT EXPECTED_STDOUT(regex, whole output) EXE [ARGS...]
check() {
    local name=$1 want_code=$2 want_out=$3; shift 3
    local out code err
    err=$(mktemp)
    # A CRT's text-mode stdout writes CRLF, as on Windows; compared without
    # the CRs.
    # shellcheck disable=SC2086
    out=$(cd "$(dirname "$1")" && timeout 120 "$gexc" run $flags ${extra:-} "$@" 2>"$err"; echo "exit=$?")
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

# check_gl NAME EXPECTED_EXIT EXPECTED_STDOUT EXE [ARGS...]: check, with the
# host's GL (`gexc run --gl`).
check_gl() {
    local saved=$flags
    flags="$flags --gl"
    check "$@"
    flags=$saved
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
    # Locks under contention (x64's SList is cmpxchg16b, done by the host
    # under one lock). Its hangs were a stale block dispatching back to
    # itself (f54d5717, and under CPU load the PAGE_STALE catch-up); a loaded
    # machine is what shows them: tests/nt/stress.sh.
    check $arch/mtx 0 "$allok" "$b/mtx.exe"
    check $arch/crtmem 0 "$allok" "$b/crtmem.exe"
    # A locked write through a null pointer's field is an access violation.
    check $arch/lownull-xadd 0 "$allok" "$b/lownull.exe" xadd
    check $arch/lownull-aligned 0 "$allok" "$b/lownull.exe" aligned
    [ $arch = x64 ] && check $arch/lownull-cmpxchg16b 0 "$allok" "$b/lownull.exe" cmpxchg16b
    check $arch/lowmem 0 "$allok" "$b/lowmem.exe"
    check $arch/apc 0 "$allok" "$b/apc.exe"
    check $arch/tls 0 "$allok" "$b/tls.exe"
    check $arch/cxx 0 "$allok" "$b/cxx.exe"
    # Hot indirect calls (devirtualised under --cgen, with the profile above).
    check $arch/icall 0 "icall: 1284803844 -1238658015 2 3 4" "$b/icall.exe"
    # N4: USER without a display.
    check $arch/user_win 0 "$allok" "$b/user_win.exe"
    check $arch/gdi 0 "$allok" "$b/gdi.exe"
    check $arch/input 0 "$allok" "$b/input.exe"
    check $arch/filemap 0 "$allok" "$b/filemap.exe"
    # crypt32 with gex's empty Unix side, and the registered RSA provider.
    check $arch/crypt 0 "$allok" "$b/crypt.exe"
    # Anonymous and named pipes within the process (CreatePipe).
    check $arch/pipe 0 "$allok" "$b/pipe.exe"
    # A 64-bit guest memory (docs/mem64.md): Unity's 12 GiB of reservations.
    if [ $arch = x64 ]; then
        extra=--mem64 check $arch/mem64 0 "$allok" "$b/mem64.exe"
    fi
    # Display modes, D3DKMT, memory status, a section mapped twice, dialogs.
    check $arch/display 0 "$allok" "$b/display.exe"
    # An image without DYNAMIC_BASE: at its linked base, or relocated by ntdll
    # with its header as linked.
    check $arch/fixedbase 0 "$allok" "$b/fixedbase.exe"
    check $arch/fixedbase-taken 0 "$allok" "$b/fixedbase.exe" taken
    # opengl32 through the generated transport (tools/gen_gl_compat.py).
    check_gl $arch/glcompat 0 "$allok" "$b/glcompat.exe"
    # A window that paints and runs a message loop (tests/nt/browser-gui.mjs shows it).
    check $arch/window 0 "shown${nl}bye" "$b/window.exe" 0
    # Code written, rewritten and reallocated at run time (a JIT).
    check $arch/jitcode 0 "$allok" "$b/jitcode.exe"
    # A function's prologue patched at run time, as Detours does.
    check $arch/detour 0 "$allok" "$b/detour.exe"
    # A block over two pages, only the second rewritten (a hang, once).
    check $arch/pagespan 0 "$allok" "$b/pagespan.exe"
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
