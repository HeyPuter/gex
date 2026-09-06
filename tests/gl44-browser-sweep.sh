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
# half for the set. Do not edit the tree while it runs: every probe rebuilds
# the guest driver, so a half-finished change is compiled mid-sweep and the
# failures that follow look like a mass regression rather than a build error.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-/tmp/gl44-browser-sweep}
mkdir -p "$OUT"
: > "$OUT/results.tsv"

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
