#!/bin/sh
# The browser CTS, family by family, as a table.
#
#   tests/gl44-cts-families.sh [family ...]
#
# tests/gl44-cts-browser.sh runs one case set and says whether it passed.
# Answering "where is the browser behind the native driver" meant running it
# by hand once per family and keeping the numbers somewhere, which is how a
# gap table goes stale. This prints one line per family and a total, and
# GEX_CTS_NATIVE=1 runs the same list through tests/gl-cts-native.sh instead,
# so the two columns come from the same list in the same order.
#
# A family is worth looking at when the two disagree. A family both fail is a
# driver question and belongs in the native suite; a family only the browser
# fails is the guest, the transport or the decoder, and nothing but this finds
# those -- KHR-GL44.compute_shader sat at 3 of 42 against 38 natively, all of
# it one decoder bug, and no probe or app gate had ever said so.
#
# GEX_CTS_PREFIX picks the suite, KHR-GL44 by default. A family that does not
# exist under the prefix asked for reports NOCASES rather than a clean zero:
# direct_state_access is a GL 4.5 family and answered "0 pass 0 fail" here,
# which reads exactly like a family with nothing wrong with it.
#
# The idle allowance is raised well above tests/gl44-cts-browser.sh's own
# default. A whole family in one browser session goes quiet for long stretches
# while the guest JIT translates -- explicit_uniform_location is silent for
# over two minutes and then passes all 48 of its cases in 173 seconds. At the
# 120-second default it was killed as a hang and reported NORESULT, which is
# indistinguishable from a family that crashes.
#
# Roughly two minutes a family with a warm chromium. Do not edit the tree while
# it runs: every case set rebuilds the guest driver.
set -u
ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)

FAMILIES=${*:-}
if [ -z "$FAMILIES" ]; then
    FAMILIES="compute_shader shader_image_load_store shader_atomic_counters
              texture_gather texture_view program_interface_query
              explicit_uniform_location shader_storage_buffer_object
              vertex_attrib_binding buffer_storage shading_language_420pack
              gpu_shader5 transform_feedback shader_subroutine
              sample_variables cull_distance"
fi

# Each family's full output is kept. Without this a sweep runs for an hour and
# throws away the only copy of the driver's own refusal messages, so answering
# "why did those sixteen fail" costs another hour.
LOGDIR=${GEX_CTS_LOGDIR:-$ROOT/web/.work/gl44-families/$(date +%Y%m%d-%H%M%S)}
mkdir -p "$LOGDIR"
echo "per-family logs: $LOGDIR"

printf '%-38s %6s %6s %6s\n' family pass fail skip
total_pass=0 total_fail=0 total_skip=0
for family in $FAMILIES; do
    case_set="${GEX_CTS_PREFIX:-KHR-GL44}.$family.*"
    if [ -n "${GEX_CTS_NATIVE:-}" ]; then
        out=$("$ROOT/tests/gl-cts-native.sh" --deqp-case="$case_set" 2>&1)
        pass=$(printf '%s' "$out" | sed -n 's/.*Passed: *\([0-9]*\)\/.*/\1/p' | tail -1)
        fail=$(printf '%s' "$out" | sed -n 's/.*Failed: *\([0-9]*\)\/.*/\1/p' | tail -1)
        skip=$(printf '%s' "$out" | sed -n 's/.*Not supported: *\([0-9]*\)\/.*/\1/p' | tail -1)
        printf '%s\n' "$out" > "$LOGDIR/$family.log"
    else
        out=$(GEX_CTS_CASE="$case_set" \
              GEX_CTS_IDLE=${GEX_CTS_IDLE:-420} \
              GEX_CTS_WALL=${GEX_CTS_WALL:-1500} \
              "$ROOT/tests/gl44-cts-browser.sh" 2>&1)
        # The harness echoes the page's stdout twice, once live and once in
        # the replayed log, so the last QPA-TOTAL is the whole answer and the
        # earlier ones are the same line again.
        line=$(printf '%s' "$out" | grep -oE 'QPA-TOTAL pass=[0-9]+ fail=[0-9]+ skip=[0-9]+' | tail -1)
        pass=$(printf '%s' "$line" | sed -n 's/.*pass=\([0-9]*\).*/\1/p')
        fail=$(printf '%s' "$line" | sed -n 's/.*fail=\([0-9]*\).*/\1/p')
        printf '%s\n' "$out" > "$LOGDIR/$family.log"
        skip=$(printf '%s' "$line" | sed -n 's/.*skip=\([0-9]*\).*/\1/p')
    fi
    # No numbers at all is not zero failures: the run did not produce a
    # result, and printing 0/0/0 would read as a clean family.
    if [ -z "${pass:-}" ]; then
        printf '%-38s %6s %6s %6s\n' "$family" - - 'NORESULT'
        continue
    fi
    # Zero of everything is not a clean family, it is a family the suite does
    # not have under this prefix.
    if [ "$pass" -eq 0 ] && [ "${fail:-0}" -eq 0 ] && [ "${skip:-0}" -eq 0 ]; then
        printf '%-38s %6s %6s %6s\n' "$family" - - 'NOCASES'
        continue
    fi
    printf '%-38s %6s %6s %6s\n' "$family" "$pass" "$fail" "${skip:-0}"
    total_pass=$((total_pass + pass))
    total_fail=$((total_fail + ${fail:-0}))
    total_skip=$((total_skip + ${skip:-0}))
done
printf '%-38s %6s %6s %6s\n' TOTAL "$total_pass" "$total_fail" "$total_skip"
