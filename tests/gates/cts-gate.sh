#!/bin/bash
# A handful of fast CTS families, run as a regression gate.
#
# The probe suite, SuperTuxKart and Blender are the standing gates and they are
# not enough on their own: a buffer-growth regression -- gex_wgpu_ensure_buffer
# comparing the padded allocation instead of width0 -- lived through all three
# for two sessions, because none of them grows a driver-internal buffer past a
# 16-byte boundary. It cost KHR-GL45.transform_feedback thirteen cases, and one
# run of that family would have caught it the day it landed.
#
# These are chosen to be quick (a couple of minutes each at most) and to lean
# on the driver's *internal* buffers -- the capture passes, the geometry and
# tessellation index buffers -- which is where the probes are thin.
#
# Prints got/total against the baseline last measured. A number below its
# baseline is a regression; above is progress, and the baseline here should be
# updated in the same commit that earns it.
set -u
cd /home/velzie/src/gex2 || exit 2
run() {
    local fam=$1 want=$2
    local got start
    start=$SECONDS
    got=$(DISPLAY=${GEX_DISPLAY:-:78} timeout -s KILL 1800 \
          bash tests/gl-cts-native.sh --deqp-case="KHR-GL45.$fam.*" 2>&1 |
          grep -aoE "Passed: *[0-9]+/[0-9]+" | head -1 | tr -s ' ')
    [ -z "$got" ] && got="Passed: (no result)"
    printf '%-24s %-16s baseline=%-8s %ss\n' "$fam" "${got#Passed: }" "$want" \
           "$((SECONDS - start))"
}
# Each of these runs in about a minute. texture_swizzle was tried here and
# taken out again: it is thousands of cases and turns the gate into a
# half-hour job, which is a thing nobody runs.
run transform_feedback   19/21
run transform_feedback2  1/1
# 2 of 3: the third is NotSupported, not failing. A baseline is what the run
# actually prints, not what it ought to be.
run transform_feedback3  2/3
run texture_view         3/7
run geometry_shader      124/136
