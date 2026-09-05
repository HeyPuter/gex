#!/bin/bash
# Group an app sweep's driver messages by cause.
#
#   sweep-causes.sh [sweepdir]
#
# The sweep's own summary compares a verdict against the reference's verdict,
# which answers "is this the driver or the machine" and nothing else. What
# decides the next fix is *why* the driver complained, across all 341 programs
# at once -- one cause behind thirty programs is worth more than thirty
# unrelated ones, and that ranking is invisible in a per-program table.
#
# Numbers are normalised away so that the same complaint about different slots,
# formats or sizes collapses into one line.
set -u
D=${1:-/tmp/sweep-wm}
[ -d "$D" ] || { echo "no sweep at $D" >&2; exit 2; }

echo "=== driver complaints, by cause (gex_wgpu runs only) ==="
# Format probing is routine: st/mesa asks about every format at startup and a
# "render target refused" is the honest answer, not a complaint.
grep -h "gex-wgpu" "$D"/*.log 2>/dev/null |
  grep -aiE "reject|refus|declin|unsupported|cannot|failed" |
  grep -av "render target refused" |
  sed 's/[0-9][0-9]*/N/g; s/0x[0-9a-fN]*/Nx/g' |
  sort | uniq -c | sort -rn | head -25

echo
echo "=== how many distinct programs each cause reaches ==="
# A cause in one program may be that program's own oddity; a cause in twenty is
# a driver gap. Counting programs, not lines, is what separates them.
for f in "$D"/*.log; do
  [ -e "$f" ] || continue
  case "$f" in *.ref.log) continue;; esac
  grep -aiE "gex-wgpu.*(reject|refus|declin|unsupported|cannot|failed)" "$f" 2>/dev/null |
    grep -av "render target refused" |
    sed 's/[0-9][0-9]*/N/g; s/0x[0-9a-fN]*/Nx/g' | sort -u |
    sed "s|^|$(basename "$f" .log)\t|"
done | cut -f2- | sort | uniq -c | sort -rn | head -25

echo
echo "=== verdicts where the reference disagrees, which is the only evidence ==="
awk -F'\t' 'NR>1 && $4=="YES"{printf "  %-32s gex=%-11s ref=%-11s %s\n", $1, $2, $3, substr($6,1,60)}' \
  "$D/results.tsv" 2>/dev/null
