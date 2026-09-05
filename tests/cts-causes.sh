#!/bin/bash
# Run CTS families and group the driver's refusals by cause.
#
#   tests/cts-causes.sh <outdir> <family> [family...]
#   tests/cts-causes.sh /tmp/causes geometry_shader tessellation_shader
#
# The counterpart to tests/gates/sweep-causes.sh, for the CTS instead of the
# app sweep, and for the same reason: a family's pass rate says how much is
# wrong, never *what*. One cause behind forty cases is worth more than forty
# unrelated ones, and that ranking is invisible in a score.
#
# Each family is run once and its whole stderr kept, so a cause can be traced
# back to the family it came from afterwards. Numbers are normalised away so
# that the same complaint about different slots, formats or sizes collapses
# into one line.
#
# One family at a time, deliberately. Four concurrent CTS runs once competed
# for this machine's single GPU for an hour and a half and truncated each
# other's output; every measurement from that window had to be taken again.
set -u
[ $# -ge 2 ] || { echo "usage: $0 <outdir> <family> [family...]" >&2; exit 2; }
ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
OUT=$1; shift
mkdir -p "$OUT"

for family in "$@"; do
    log="$OUT/$family.log"
    printf '%-34s ' "$family"
    timeout -s KILL 3600 bash "$ROOT/tests/gl-cts-native.sh" \
        --deqp-case="KHR-GL46.$family.*" >"$log" 2>&1
    grep -aoE "Passed: *[0-9]+/[0-9]+" "$log" | head -1 | tr -s ' ' ||
        echo "(no result)"
done

echo
echo "=== driver complaints, by cause ==="
# Format probing is routine: st/mesa asks about every format at startup and a
# "render target refused" is the honest answer, not a complaint.
grep -h "gex-wgpu" "$OUT"/*.log 2>/dev/null |
  grep -aiE "reject|refus|declin|unsupported|cannot|failed|gave up" |
  grep -av "render target refused" |
  sed 's/[0-9][0-9]*/N/g; s/0x[0-9a-fN]*/Nx/g' |
  sort | uniq -c | sort -rn | head -30

echo
echo "=== which families each cause reaches ==="
# A cause in one family may be that family's own corner; a cause in five is a
# driver gap. This is the column that picks the next fix.
for f in "$OUT"/*.log; do
    [ -e "$f" ] || continue
    grep -aiE "gex-wgpu.*(reject|refus|declin|unsupported|cannot|failed)" "$f" 2>/dev/null |
      grep -av "render target refused" |
      sed 's/[0-9][0-9]*/N/g; s/0x[0-9a-fN]*/Nx/g' | sort -u |
      sed "s|^|$(basename "$f" .log)\t|"
done | cut -f2- | sort | uniq -c | sort -rn | head -30
