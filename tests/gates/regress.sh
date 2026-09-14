#!/bin/bash
# The standard regression set, in one command, against recorded baselines.
#
#   tests/gates/regress.sh              # build, install, probe suite, families
#   tests/gates/regress.sh --no-build   # use whatever is installed
#   tests/gates/regress.sh --probes     # probe suite only (~7 min)
#   tests/gates/regress.sh --families   # CTS families only (~15 min)
#
# This exists because the same set was assembled by hand four times in one
# afternoon, and assembling it by hand is where two of the day's mistakes came
# from: a build that did not install, and a family score compared against a
# number remembered rather than recorded.
#
# The baselines live in regress-baseline.txt, one `key expected` line each, so
# raising one is an edit with a diff rather than a number in a script. Raise a
# baseline in the same commit that earns it.
#
# What this is NOT: a statement about the browser. Every number here is native.
# tests/gl44-browser-sweep.sh is the browser's own instrument and the one that
# counts -- see the handoff's "the browser path is the one that counts".
set -u
ROOT=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
BASE=${GEX_REGRESS_BASELINE:-$ROOT/tests/gates/regress-baseline.txt}
INST=${GEX_INST:-/tmp/gex-mesa-native/inst}
WORK=${GEX_REGRESS_WORK:-${TMPDIR:-/tmp}/gex-regress}
mkdir -p "$WORK"

do_build=1 do_probes=1 do_families=1
for arg in "$@"; do
  case "$arg" in
    --no-build) do_build=0 ;;
    --probes)   do_families=0 ;;
    --families) do_probes=0 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done

fails=0
say() { printf '%-44s %s\n' "$1" "$2"; }
# A measurement that produced no number is not a pass. Every check here goes
# through this so that "no result" can never read as "nothing changed" -- the
# failure mode a-measurement-can-measure-nothing is about.
check() {  # check <key> <got>
  local key=$1 got=$2 want
  want=$(awk -v k="$key" '$1==k{print $2; found=1} END{if(!found) print "?"}' "$BASE")
  if [ "$want" = "?" ]; then
    say "$key" "NO BASELINE (got $got) -- add it to $(basename "$BASE")"
    fails=$((fails + 1)); return
  fi
  if [ -z "$got" ]; then
    say "$key" "NORESULT (expected $want)"
    fails=$((fails + 1)); return
  fi
  if [ "$got" = "$want" ]; then
    say "$key" "ok ($got)"
  else
    say "$key" "CHANGED got=$got expected=$want"
    fails=$((fails + 1))
  fi
}

if [ "$do_build" = 1 ]; then
  echo "== build =="
  if ! "$ROOT/web/build-mesa-native.sh" > "$WORK/build.log" 2>&1; then
    grep -iE ' error|FAILED' "$WORK/build.log" | head -20
    echo "build FAILED; see $WORK/build.log"; exit 1
  fi
  # The driver is libgallium-*.so. libdril_dri.so is a stub, and checking its
  # timestamp made several clean builds read as stale.
  drv=$INST/lib/libgallium-26.3.0-devel.so
  if [ ! -f "$drv" ]; then
    echo "no driver installed at $drv"; exit 1
  fi
  src=$ROOT/third_party/mesa/src/gallium/drivers/gex_wgpu/gex_wgpu_screen.c
  if [ "$src" -nt "$drv" ]; then
    echo "the driver source is newer than the installed driver -- the install did not happen"
    exit 1
  fi
  say "build+install" "ok ($(date -r "$drv" +%H:%M:%S))"
fi

if [ "$do_probes" = 1 ]; then
  echo "== native probe suite =="
  bash "$ROOT/tests/gates/probe-suite.sh" > "$WORK/probes.txt" 2>&1
  ok=$(grep -c ' OK ' "$WORK/probes.txt")
  check probe_suite_ok "$ok"
  # The expected non-OK lines are listed by name rather than counted: a count
  # stays right when one known exception is fixed and a new one appears.
  unexpected=$(grep -vE ' OK ' "$WORK/probes.txt" |
               grep -vE "^($(awk '$1=="probe_expected_diff"{printf "%s%s", sep, $2; sep="|"}' "$BASE")) ")
  if [ -n "$unexpected" ]; then
    say "probe_suite_exceptions" "NEW:"
    printf '%s\n' "$unexpected" | sed 's/^/    /'
    fails=$((fails + 1))
  else
    say "probe_suite_exceptions" "ok (only the known ones)"
  fi
fi

if [ "$do_families" = 1 ]; then
  echo "== CTS families (native) =="
  while read -r key want; do
    case "$key" in cts_*) ;; *) continue ;; esac
    family=${key#cts_}
    out=$(GEX_INST="$INST" "$ROOT/tests/gl-cts-native.sh" \
            --deqp-case="KHR-GL45.$family.*" 2>&1)
    printf '%s\n' "$out" > "$WORK/$family.log"
    pass=$(printf '%s' "$out" | sed -n 's/.*Passed: *\([0-9]*\)\/.*/\1/p' | tail -1)
    fail=$(printf '%s' "$out" | sed -n 's/.*Failed: *\([0-9]*\)\/.*/\1/p' | tail -1)
    # Counted whole, never grepped for a message: an A/B that grepped for the
    # text once looked causal and was luck on both sides.
    errs=$(printf '%s' "$out" | grep -c 'WebGPU error')
    [ -n "$pass" ] && [ -n "$fail" ] && got="$pass/$fail" || got=""
    check "$key" "$got"
    [ "${errs:-0}" -gt 0 ] && say "  $family webgpu_errors" "$errs (were they there before?)"
  done < "$BASE"
fi

echo
if [ "$fails" = 0 ]; then
  echo "regress: OK"
else
  echo "regress: $fails check(s) moved -- read each before deciding it is a regression"
fi
exit $((fails > 0))
