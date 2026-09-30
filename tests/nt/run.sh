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
fx=$root/crates/gex-ld/tests/pe/bin
[ -x "$gexc" ] || { echo "no gexc at $gexc" >&2; exit 2; }
prefix=${GEX_WINE_PREFIX:-$HOME/.cache/gex/wine-11.15}
[ -f "$prefix/.wine-version" ] || { echo "no staged Wine prefix at $prefix; run tools/nt-stage-wine.sh" >&2; exit 2; }
export GEX_NT_STRICT=1

pass=0 fail=0
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

nl=$'\n'
check hello_args 42 "hello from a Windows program with 3 args${nl}arg 1: a${nl}arg 2: b c" \
    "$here/bin/hello_args.exe" a "b c"
check pe-hello 0 "Hello from x64 PE!" "$fx/x64/hello.exe"
check pe-msvc 9 "msvc" "$fx/x64/msvc.exe"
check pe-highbase 0 "tiny" "$fx/x64/highbase.exe"
check pe-uses 0 "229 [0-9A-F]{16} 11" "$fx/x64/uses.exe"
check pe-threads 0 "" "$fx/x64/threads.exe"
# N2: every line "ok ...", then "0 failures".
allok="(ok [^$nl]*$nl)+0 failures"
check sync 0 "$allok" "$here/bin/sync.exe"
check apc 0 "$allok" "$here/bin/apc.exe"
check tls 0 "$allok" "$here/bin/tls.exe"
check cxx 0 "$allok" "$here/bin/cxx.exe"
# Ends in the top-level exception filter, which exits with 77.
check seh 77 "(ok [^$nl]*$nl)+top-level filter saw e00000ff${nl}0 failures" "$here/bin/seh.exe"

echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
