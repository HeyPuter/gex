#!/bin/bash
# Every gl44 probe, through a real browser.
#
#   tests/gl44-browser-sweep.sh [outdir]
#
# tests/gates/probe-suite.sh compares the driver against llvmpipe *natively*.
# This is the other instrument: tests/gl44-probe.sh builds each probe twice --
# once natively for an oracle, once for the guest -- and runs the guest half in
# chromium, demanding the same result. Two classes of defect are reachable only
# here, because the native path goes through the system libGL and never through
# the guest marshaller:
#
#   - the JS binding layer. An emdawnwebgpu enum typo spelled a GPUBlendFactor
#     `src1alpha` instead of `src1-alpha`, so dual-source blending threw inside
#     wgpuDeviceCreateRenderPipeline. Native Dawn takes the C enum and never
#     builds a string, so no native gate could ever see it.
#   - the guest marshaller. A missing entry point makes gen_gl_extensions.py
#     withhold the whole extension, and a probe that asks for it then reports
#     cases=0 -- a well-formed SUMMARY that passes any check looking only at
#     `failed=`. Compare case *counts* with the oracle, which this does.
#
# About forty seconds a probe with a warm chromium, so roughly an hour and a
# half for the set.
#
# It rebuilds each probe's guest *binary* and the guest libGL, and **copies**
# an already-built host wasm module. It does not build that module. This
# comment used to claim "every probe rebuilds the guest driver", which is
# false and had the consequence backwards twice over: editing the driver
# mid-sweep does not corrupt the run, and running a sweep after editing
# without web/build-mesa-wasm.sh silently measures the *old* driver.
#
# That happened on 2026-09-08. A full sweep reported 191 PASS and 4 FAIL
# against a driver three commits behind; three of the four "failures" were the
# staleness and vanished on a rebuild. gl44-probe.sh had said so all along --
# "gex_wgpu_screen.c is newer than the staged host module" appeared in **195
# of the 197** per-probe logs and in none of this script's own output, because
# each probe's stderr goes to its own file. An instrument nobody sees is not
# an instrument.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-/tmp/gl44-browser-sweep}
mkdir -p "$OUT"
: > "$OUT/results.tsv"

# So the check happens once, here, where it is visible -- and refuses rather
# than warns. Two hours of verdicts against the wrong driver is worse than
# stopping: every PASS is meaningless and every FAIL sends someone after a bug
# that is not there. GEX_ALLOW_STALE=1 for the case where measuring the older
# driver is the point.
MESA_BUILD=${GEX_MESA_WASM_OUT:-${TMPDIR:-/tmp}/gex-mesa-wasm}/build/src/gallium/targets/gex-wasm
STAGED=$MESA_BUILD/gex-wgpu-replay.wasm
DRIVER_SRC=$ROOT/third_party/mesa/src/gallium/drivers/gex_wgpu
if [ "${GEX_ALLOW_STALE:-0}" != 1 ]; then
    if [ ! -f "$STAGED" ]; then
        echo "no host wasm module at $STAGED -- run web/build-mesa-wasm.sh" >&2
        exit 2
    fi
    for src in "$DRIVER_SRC"/gex_wgpu_screen.c "$DRIVER_SRC"/gex_wgpu_native.c; do
        [ -f "$src" ] || continue
        if [ "$src" -nt "$STAGED" ]; then
            echo "$(basename "$src") is newer than the staged host module." >&2
            echo "Run web/build-mesa-wasm.sh (and, for the page, cd web &&" \
                 "npm run build) first, or GEX_ALLOW_STALE=1 to sweep the" \
                 "older driver deliberately." >&2
            exit 2
        fi
    done
fi

for source in "$ROOT"/tests/integration/programs/gl44-*-probe.c; do
    name=$(basename "$source" | sed 's|^gl44-||; s|-probe\.c$||')
    log="$OUT/$name.log"
    timeout 400 sh "$ROOT/tests/gl44-probe.sh" "$name" > "$log" 2>&1
    status=$?
    case $status in
        0) verdict=PASS ;;
        # gl44-probe.sh exits 3 for a probe the guest toolchain cannot build at
        # all -- the GLX ones, which need headers the aarch64-musl cross
        # compiler has not got. An explicit verdict, so they do not sit in the
        # FAIL column beside real defects.
        3) verdict=SKIP ;;
        124) verdict=TIMEOUT ;;
        *) verdict=FAIL
           grep -qE "no such probe|BUILDFAIL" "$log" && verdict=NOBUILD
           grep -qE "cdp:.*did not answer|chrome did not expose" "$log" &&
               verdict=HARNESS ;;
    esac
    printf '%s\t%s\n' "$name" "$verdict" >> "$OUT/results.tsv"
    printf '%-28s %s\n' "$name" "$verdict"
done

echo
echo "--- $OUT/results.tsv"
cut -f2 "$OUT/results.tsv" | sort | uniq -c
# A HARNESS or TIMEOUT verdict is about the machine, not the driver; see the
# stale-process check in docs/gl-webgpu-handoff.md before reading anything into
# a run that has them.
