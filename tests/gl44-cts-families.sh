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
# Roughly a minute a family with a warm chromium. Do not edit the tree while
# it runs: every case set rebuilds the guest driver.
set -u
ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)

FAMILIES=${*:-}
if [ -z "$FAMILIES" ]; then
    FAMILIES="compute_shader shader_image_load_store shader_atomic_counters
              texture_gather texture_view program_interface_query
              explicit_uniform_location shader_storage_buffer_object
              vertex_attrib_binding buffer_storage shading_language_420pack
              enhanced_layouts gpu_shader5 transform_feedback
              shader_subroutine sample_variables cull_distance
              direct_state_access"
fi

printf '%-38s %6s %6s %6s\n' family pass fail skip
total_pass=0 total_fail=0 total_skip=0
for family in $FAMILIES; do
    case_set="KHR-GL44.$family.*"
    if [ -n "${GEX_CTS_NATIVE:-}" ]; then
        out=$("$ROOT/tests/gl-cts-native.sh" --deqp-case="$case_set" 2>&1)
        pass=$(printf '%s' "$out" | sed -n 's/.*Passed: *\([0-9]*\)\/.*/\1/p' | tail -1)
        fail=$(printf '%s' "$out" | sed -n 's/.*Failed: *\([0-9]*\)\/.*/\1/p' | tail -1)
        skip=$(printf '%s' "$out" | sed -n 's/.*Not supported: *\([0-9]*\)\/.*/\1/p' | tail -1)
    else
        out=$(GEX_CTS_CASE="$case_set" "$ROOT/tests/gl44-cts-browser.sh" 2>&1)
        # The harness echoes the page's stdout twice, once live and once in
        # the replayed log, so the last QPA-TOTAL is the whole answer and the
        # earlier ones are the same line again.
        line=$(printf '%s' "$out" | grep -oE 'QPA-TOTAL pass=[0-9]+ fail=[0-9]+ skip=[0-9]+' | tail -1)
        pass=$(printf '%s' "$line" | sed -n 's/.*pass=\([0-9]*\).*/\1/p')
        fail=$(printf '%s' "$line" | sed -n 's/.*fail=\([0-9]*\).*/\1/p')
        skip=$(printf '%s' "$line" | sed -n 's/.*skip=\([0-9]*\).*/\1/p')
    fi
    # No numbers at all is not zero failures: the run did not produce a
    # result, and printing 0/0/0 would read as a clean family.
    if [ -z "${pass:-}" ]; then
        printf '%-38s %6s %6s %6s\n' "$family" - - 'NORESULT'
        continue
    fi
    printf '%-38s %6s %6s %6s\n' "$family" "$pass" "$fail" "${skip:-0}"
    total_pass=$((total_pass + pass))
    total_fail=$((total_fail + ${fail:-0}))
    total_skip=$((total_skip + ${skip:-0}))
done
printf '%-38s %6s %6s %6s\n' TOTAL "$total_pass" "$total_fail" "$total_skip"
