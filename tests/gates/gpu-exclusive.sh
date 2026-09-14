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
    gex_theirs=
    for gex_name in chromium chrome glcts deqp-gl; do
        # No `|| echo 0`: pgrep -c prints 0 *and* exits non-zero when it
        # matches nothing, so the fallback appended a second line and made
        # the test below an "integer expected" error -- which, being on
        # stderr and non-fatal, let the guard pass silently.
        gex_n=0
        gex_mine=0
        for gex_pid in $(pgrep -x "$gex_name" 2>/dev/null); do
            gex_n=$((gex_n + 1))
            # Ours or the user's? This runs on somebody's desktop, and the
            # browser this project drives is headless while the one a person
            # is reading with is not. Classified by reading the matched pid's
            # own /proc entry rather than by `pgrep -f`, which would match
            # this guard's shell -- pgrep-f-matches-the-waiter, again.
            if tr '\0' ' ' < "/proc/$gex_pid/cmdline" 2>/dev/null |
                   grep -q -- '--headless\|ozone-platform=headless'; then
                gex_mine=$((gex_mine + 1))
            fi
        done
        [ "$gex_n" -gt 0 ] &&
            gex_busy="$gex_busy $gex_name($gex_n)"
        [ "$gex_n" -gt "$gex_mine" ] &&
            gex_theirs="$gex_theirs $gex_name($((gex_n - gex_mine)))"
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
    echo "  A browser or CTS process holds GPU resources and makes this" >&2
    echo "  measurement several times slower without saying so." >&2
    if [ -n "$gex_theirs" ]; then
        # The important half. This guard used to print "clear them and
        # re-run: killmatch.py chromium" for *any* chromium, and
        # killmatch.py SIGKILLs everything whose command line contains the
        # word -- which on a desktop is the browser somebody is reading
        # with. Advising that was a destructive suggestion dressed up as
        # housekeeping, and it was shipped before anyone hit the case.
        echo "" >&2
        echo "  NOT OURS: $gex_theirs is not headless, so it is a browser" >&2
        echo "  somebody is using. Do not kill it. Either wait for it to" >&2
        echo "  close, or re-run with GEX_ALLOW_BUSY_GPU=1 and treat every" >&2
        echo "  timing this produces as unreliable -- a photographed gate" >&2
        echo "  under a live desktop measures the desktop too." >&2
    else
        echo "  These are ours -- headless, left over from an earlier run." >&2
        echo "  Clear them and re-run:" >&2
        echo "" >&2
        echo "    python3 tests/gates/killmatch.py -- --ozone-platform=headless" >&2
        echo "    python3 tests/gates/killmatch.py drive.mjs" >&2
    fi
    echo "" >&2
    echo "  GEX_ALLOW_BUSY_GPU=1 proceeds anyway." >&2
    return 1
}
