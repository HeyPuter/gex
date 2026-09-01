#!/bin/sh
# Sweep official OpenGL conformance families through the browser and record
# what each one does.
#
#   tests/gl44-cts-sweep.sh [family ...]
#
# GEX_CTS_MODULE and GEX_CTS_MUSTPASS choose the GL version, and
# GEX_CTS_BACKEND chooses which host backend executes the guest's stream --
# `mesa` for Mesa on the gex_wgpu Gallium driver, MobileGL otherwise. For a
# GL 3.2 sweep of the Mesa backend:
#
#   GEX_CTS_BACKEND=mesa GEX_CTS_MODULE=KHR-GL32 \
#   GEX_CTS_MUSTPASS=.../mustpass/gl/khronos_mustpass/main/gl32-main.txt \
#   GEX_CTS_SWEEP_OUT=web/.work/gl32-sweep-mesa tests/gl44-cts-sweep.sh
#
# With no arguments it sweeps every family named by the pinned Khronos
# `gl44-main.txt` mustpass list. Named families run just those.
#
# The point is a landscape, not a verdict. "Fully compliant to OpenGL 4.4"
# means 18,585 cases pass; before any of them can be fixed it has to be clear
# which families produce real GL results at all, and which die for reasons that
# are not GL -- a runtime trap takes a whole family with it and would otherwise
# be indistinguishable from thousands of ordinary failures.
#
# Results are appended to a TSV as each family finishes, so an interrupted
# sweep keeps what it learned and a rerun skips what it already has.
#
#   family  status  passed  total  seconds  note
#
# status is one of:
#   ok       every case ran and none failed (some may be "not supported",
#            which the CTS reports separately and which is not a defect)
#   absent   the mustpass names this family but this glcts build has no such
#            cases -- nothing ran, and nothing is wrong with the driver
#   fail     every case ran; some failed
#   trap     the gex runtime trapped -- the family died early, counts partial
#   timeout  the family did not finish inside the per-family wall
#   error    the harness itself failed
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
MUSTPASS=${GEX_CTS_MUSTPASS:-$ROOT/third_party/vk-gl-cts/external/openglcts/data/gl_cts/data/mustpass/gl/khronos_mustpass/main/gl44-main.txt}
OUT=${GEX_CTS_SWEEP_OUT:-$ROOT/web/.work/gl44-sweep}
RESULTS=$OUT/results.tsv
WALL=${GEX_CTS_SWEEP_WALL:-600}
# Which CTS module set to sweep. The mustpass list and the module have to name
# the same GL version, and the version has to be one the backend can actually
# give: sweeping KHR-GL44 against a context that came up 3.2 measures context
# creation, not conformance.
MODULE=${GEX_CTS_MODULE:-KHR-GL44}

[ -f "$MUSTPASS" ] || { echo "no mustpass list at $MUSTPASS" >&2; exit 1; }
mkdir -p "$OUT/logs"
[ -f "$RESULTS" ] || printf 'family\tstatus\tpassed\ttotal\tseconds\tnote\n' > "$RESULTS"

if [ "$#" -gt 0 ]; then
    families=$*
else
    families=$(cut -d. -f2 "$MUSTPASS" | sort -u)
fi

for family in $families; do
    # Resume: a family already recorded is not run again. Delete its row to
    # force a rerun.
    if cut -f1 "$RESULTS" | grep -qx "$family"; then
        continue
    fi

    log=$OUT/logs/$family.log
    started=$(date +%s)
    set +e
    GEX_CTS_SKIP_BUILD=1 GEX_CTS_CASE="$MODULE.$family.*" \
        timeout "$WALL" "$ROOT/tests/gl44-cts-browser.sh" >"$log" 2>&1
    harness=$?
    set -e
    seconds=$(( $(date +%s) - started ))

    # dEQP prints its own tally; it is the authority when the run completed.
    passed=$(sed -n 's/.*Passed:[[:space:]]*\([0-9]*\)\/\([0-9]*\).*/\1/p' "$log" | tail -1)
    total=$(sed -n 's/.*Passed:[[:space:]]*\([0-9]*\)\/\([0-9]*\).*/\2/p' "$log" | tail -1)
    # Cases the runner announced, which is how far it got when it never
    # reached a tally.
    started_cases=$(grep -c "Test case '" "$log" || true)
    note=

    if grep -q "trapped inside the shim" "$log"; then
        status=trap
        last=$(grep -o "Test case '[^']*'" "$log" | tail -1 | sed "s/Test case '//;s/'//")
        note="died in ${last:-?}"
        passed=${passed:-0}
        total=${total:-$started_cases}
    elif [ "$harness" -eq 124 ]; then
        status=timeout
        note="ran ${started_cases} cases in ${WALL}s"
        passed=${passed:-0}
        total=${total:-$started_cases}
    elif grep -q "QPA-TOTAL pass=0 fail=0 skip=0" "$log"; then
        # The runner started, finished, and matched nothing. That is a family
        # the mustpass names but this glcts build does not contain -- the
        # `shaders` group, for one, whose 2397 entries live in a package that
        # is not built here (the binary has shaders30/42/44/45 and no plain
        # `shaders`). Reporting it as an error reads as a driver problem.
        status=absent
        note="no such family in this glcts build"
        passed=0
        total=0
    elif [ -z "$total" ]; then
        status=error
        note=$(grep -m1 -e "^Error:" -e "FAIL" "$log" | cut -c1-80)
        passed=0
        total=0
    elif [ "$passed" = "$total" ]; then
        status=ok
    else
        # "not passed" is not "failed". A case the CTS skips because an
        # extension is absent is a legitimate result, and lumping the two
        # together reads as a broken family: pixelstoragemodes reported "896
        # failing" when its own totals said 0 failed and 160 not supported,
        # which is a very different thing to go and investigate.
        failed=$(sed -n 's/.*Failed:[[:space:]]*\([0-9]*\)\/.*/\1/p' "$log" | tail -1)
        skipped=$(sed -n 's/.*Not supported:[[:space:]]*\([0-9]*\)\/.*/\1/p' "$log" | tail -1)
        failed=${failed:-$((total - passed))}
        skipped=${skipped:-0}
        if [ "$failed" -eq 0 ]; then
            status=ok
            note="$skipped not supported"
        else
            status=fail
            note="$failed failing"
            [ "$skipped" -gt 0 ] && note="$note, $skipped not supported"
        fi
    fi

    printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$family" "$status" "$passed" "$total" "$seconds" "$note" >> "$RESULTS"
    printf '%-34s %-8s %6s/%-6s %4ss  %s\n' \
        "$family" "$status" "$passed" "$total" "$seconds" "$note"
done

echo
echo "---- sweep summary ----"
awk -F'\t' 'NR>1 {
    status[$2]++
    passed += $3
    total += $4
}
END {
    for (s in status) printf "  %-8s %d families\n", s, status[s]
    printf "  cases passed: %d/%d\n", passed, total
}' "$RESULTS"
echo "  full results: $RESULTS"
