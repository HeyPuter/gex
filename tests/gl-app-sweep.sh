#!/bin/bash
# Run every installed GL program briefly against gex_wgpu and classify it.
#
#   tests/gl-app-sweep.sh [outdir] [list]
#
# Build the list with, from the repo root:
#
#   for f in /usr/bin/*; do [ -x "$f" ] && [ -f "$f" ] &&
#     objdump -p "$f" 2>/dev/null |
#     grep -qE 'NEEDED.*(libGL\.so|libEGL\.so|libGLESv2)' && echo "$f"; done \
#     > /tmp/glapps.txt
#
# This machine has 336 of them: the mesa-demos and piglit-style suites plus a
# few real applications. It is the widest statement available about whether
# ordinary GL works here, and it is what found the vertex input location
# collision that was behind 17 of 27 crashes -- see the sweep section of
# docs/gl-webgpu-handoff.md, including why every crash was a consequence of an
# earlier refusal rather than a bug of its own.
#
# Do not run anything else on the GPU while this is going: a concurrent
# workload does not merely add noise, it changes how far a program gets inside
# its eight seconds and so changes its verdict.
#
# 341 of them are installed (mesa-demos and piglit-style tests). Individually
# they are trivial; together they are the broadest statement available about
# whether ordinary GL works here, and they cost seconds each. What matters is
# not the pass rate -- many need input, or a feature no one has claimed -- but
# which ones the *driver* refuses or crashes in.
set -u
S=$(dirname "$0")
OUT=${1:-/tmp/glsweep}
LIST=${2:-/tmp/glapps.txt}
mkdir -p "$OUT"
: > "$OUT/results.tsv"
printf 'app\tverdict\texit\tnote\n' >> "$OUT/results.tsv"
export DISPLAY=:77
bash "$S/ensure-display.sh" >/dev/null 2>&1
while read -r app; do
  name=$(basename "$app")
  case "$name" in
    aseprite|cemu|dolphin-emu|dolphin-emu-nogui|mpv) continue ;;
  esac
  log="$OUT/$name.log"
  timeout -s KILL 8 "$S/run.sh" "$app" >"$log" 2>&1
  rc=$?
  # The driver's own complaints, which are the point of the sweep. Format
  # probing ("render target refused") is routine and not a complaint.
  refused=$(grep -cE "gex-wgpu: .*(rejected|refused)" "$log" 2>/dev/null || echo 0)
  refused_real=$(grep -E "gex-wgpu: .*(rejected|refused)" "$log" 2>/dev/null \
                 | grep -vc "render target refused" || echo 0)
  wgpuerr=$(grep -c "WebGPU error" "$log" 2>/dev/null || echo 0)
  latched=$(grep -c "gex_wgpu context failed\|context latched" "$log" 2>/dev/null || echo 0)
  note=""
  if [ "$rc" -gt 128 ] && [ "$rc" -ne 137 ]; then
    verdict=CRASH; note="signal $((rc-128))"
  elif [ "$wgpuerr" -gt 0 ]; then
    verdict=WGPU-ERROR
    note=$(grep -oE "WebGPU error.*" "$log" | head -1 | cut -c1-110)
  elif [ "$refused_real" -gt 0 ]; then
    verdict=REFUSED
    note=$(grep -E "gex-wgpu: .*(rejected|refused)" "$log" | grep -v "render target refused" | head -1 | cut -c1-110)
  elif [ "$latched" -gt 0 ]; then
    verdict=LATCHED
  elif [ "$rc" -eq 137 ]; then
    verdict=RAN            # killed by the timeout: it was still drawing
  elif [ "$rc" -eq 0 ]; then
    verdict=OK
  else
    verdict=EXIT; note="exit $rc"
  fi
  printf '%s\t%s\t%s\t%s\n' "$name" "$verdict" "$rc" "$note" >> "$OUT/results.tsv"
done < "$LIST"
echo "--- verdicts ---"
tail -n +2 "$OUT/results.tsv" | cut -f2 | sort | uniq -c | sort -rn
