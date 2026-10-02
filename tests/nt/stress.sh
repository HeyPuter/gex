#!/bin/bash
# A tests/nt program run again and again under CPU load, for hangs that only
# a busy machine shows (thread hand-offs and spin-waits stretched by the
# scheduler).
#
#   tests/nt/stress.sh [--browser] ARCH EXE N [CORES [HOGS [TIMEOUT]]] [-- GEXC_ARGS...]
#
#   ARCH     x64 or x86 (tests/nt/bin/ARCH/EXE)
#   CORES    cores the run and the hogs share (taskset list, default 2,3)
#   HOGS     busy loops on those cores (default 2)
#   TIMEOUT  seconds per run (default 300)
#
# GEXC: the gexc to run natively (default target/release/gexc; it must be an
# x86 build, which a first run of hello_args.exe checks). Native runs set
# GEX_NT_HANG_DUMP=60, so a hang's output says where every
# thread is (its last safepoint pc, last indirect branch, registers; a pc that
# does not move between reports is a thread that is stuck). --browser runs the
# browser shim under Node (shims/browser/js/run-node.mjs, as run-browser.sh).
# Each run's output goes to $TMPDIR/gex-nt-stress/ARCH-EXE-I.txt; a run passes
# when it exits 0 and prints "0 failures".
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
browser=0
[ "${1:-}" = --browser ] && { browser=1; shift; }
pos=()
while [ $# -gt 0 ] && [ "$1" != -- ]; do pos+=("$1"); shift; done
[ "${1:-}" = -- ] && shift
arch=${pos[0]} exe=${pos[1]} n=${pos[2]} cores=${pos[3]:-2,3} hogs=${pos[4]:-2} to=${pos[5]:-300}
out=${TMPDIR:-/tmp}/gex-nt-stress
mkdir -p "$out"
prefix=${GEX_WINE_PREFIX:-$HOME/.cache/gex/wine-11.15}
path=$root/tests/nt/bin/$arch/$exe
[ -f "$path" ] || { echo "no $path" >&2; exit 2; }
gexc=${GEXC:-$root/target/release/gexc}
case $gexc in /*) ;; *) gexc=$PWD/$gexc ;; esac
if [ $browser = 0 ]; then
    [ -x "$gexc" ] || { echo "no gexc at $gexc (GEXC= names another)" >&2; exit 2; }
    # An AArch64 build fails every run the same way, which reads like a hang
    # report of 0 passed; say what it is instead.
    hello=$root/tests/nt/bin/$arch/hello_args.exe
    (cd "$(dirname "$hello")" && GEX_NT_STRICT=1 timeout 120 "$gexc" run hello_args.exe a "b c" >/dev/null 2>&1)
    r=$?
    [ $r = 42 ] || { echo "$gexc could not run $hello (exit $r, want 42): not an x86 build (cargo build --features x86)?" >&2; exit 2; }
else
    for f in gex_browser.x86.shared.wasm seed.x86.shared.wasm; do
        [ -f "$root/web/public/gex/$f" ] || { echo "no web/public/gex/$f; build with GEX_ARCH=x86 shims/browser/build.sh" >&2; exit 2; }
    done
fi
pids=()
for _ in $(seq 1 "$hogs"); do taskset -c "$cores" sh -c 'while :; do :; done' & pids+=($!); done
trap 'kill "${pids[@]}" 2>/dev/null' EXIT
export GEX_NT_STRICT=1
ok=0 bad=0
for i in $(seq 1 "$n"); do
    log=$out/$arch-${exe%.exe}-$i.txt
    start=$(date +%s)
    if [ $browser = 1 ]; then
        (cd "$root" && GEX_ARCH=x86 GEX_SHARED=1 timeout "$to" taskset -c "$cores" node shims/browser/js/run-node.mjs \
            --dir /wine="$prefix" --file "/t/$exe=$path" --argv0 "/t/$exe" --cwd /t --env GEX_NT_STRICT=1 \
            web/public/gex/seed.x86.shared.wasm "$path" "$@") > "$log" 2>&1
    else
        (cd "$(dirname "$path")" && GEX_NT_HANG_DUMP=${GEX_NT_HANG_DUMP:-60} timeout "$to" taskset -c "$cores" \
            "$gexc" run "$@" "$exe") > "$log" 2>&1
    fi
    r=$?
    t=$(( $(date +%s) - start ))
    if [ $r = 0 ] && grep -q '^0 failures' "$log"; then
        ok=$((ok + 1)); echo "ok   $arch $exe $i (${t}s)"
    else
        bad=$((bad + 1)); echo "FAIL $arch $exe $i: exit $r after ${t}s, $(grep -c '^ok' "$log") checks passed ($log)"
    fi
done
echo "$arch $exe: $ok passed, $bad failed"
[ $bad = 0 ]
