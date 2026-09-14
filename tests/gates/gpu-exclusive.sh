#!/bin/sh
# Refuse to start a long GPU measurement while something else is on the GPU.
#
#   . "$(dirname "$0")/gates/gpu-exclusive.sh"   # from tests/
#   gex_require_idle_gpu "gl-app-compare"
#
# Several harnesses here say "nothing else may touch the GPU while this runs"
# in their headers. None of them checked, and on 2026-09-14 that cost most of
# an afternoon: two headless chromium processes left over from an earlier probe
# run -- forty-eight minutes old, doing nothing -- held Vulkan resources, and
# tests/gl-app-compare.sh ran at **6.5 minutes per program** instead of 13
# seconds. A 30x slowdown of a measurement that has nothing to do with the
# browser, whose only symptom was taking longer than expected.
#
# The check is deliberately narrow: leftovers this project creates, named. A
# broad "is the GPU busy" test would fire on the user's desktop session and be
# switched off within a day.
#
# GEX_ALLOW_BUSY_GPU=1 proceeds anyway, for the case where the other workload
# is deliberate.

gex_require_idle_gpu() {
    gex_what=${1:-this measurement}
    [ -n "${GEX_ALLOW_BUSY_GPU:-}" ] && return 0

    gex_busy=
    for gex_name in chromium chrome glcts deqp-gl; do
        # No `|| echo 0`: pgrep -c prints 0 *and* exits non-zero when it
        # matches nothing, so the fallback appended a second line and made
        # the test below an "integer expected" error -- which, being on
        # stderr and non-fatal, let the guard pass silently.
        gex_n=$(pgrep -c -x "$gex_name" 2>/dev/null)
        [ -z "$gex_n" ] && gex_n=0
        [ "$gex_n" -gt 0 ] && gex_busy="$gex_busy $gex_name($gex_n)"
    done
    # No `pgrep -f` here, and that is deliberate. A pattern match against full
    # command lines finds *this guard's own shell*, whose command line
    # contains the pattern -- so the check fired on an idle machine and the
    # message named a process that was the caller. pgrep-f-matches-the-waiter
    # is an old lesson and it caught me writing a new instance of it.
    #
    # `pgrep -x` above matches the process *name*, which cannot match a shell
    # running a script that mentions it. And a listening harness server is not
    # a GPU consumer in any case: it holds a chromium open, and the chromium
    # is what this checks for.

    [ -z "$gex_busy" ] && return 0

    echo "$gex_what: refusing to start --$gex_busy still running." >&2
    echo "  A leftover browser or CTS process holds GPU resources and makes" >&2
    echo "  this measurement several times slower without saying so. Clear" >&2
    echo "  them and re-run:" >&2
    echo "" >&2
    echo "    python3 tests/gates/killmatch.py chromium" >&2
    echo "    python3 tests/gates/killmatch.py drive.mjs" >&2
    echo "" >&2
    echo "  GEX_ALLOW_BUSY_GPU=1 proceeds anyway." >&2
    return 1
}
