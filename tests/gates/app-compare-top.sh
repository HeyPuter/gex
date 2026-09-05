#!/bin/bash
# Re-measure the worst rows of a pixel comparison with both extra references.
#
#   app-compare-top.sh <results.tsv> [count] [outdir]
#
# The cheap sweep reports one number, and one number cannot tell a driver
# defect from an animation: both runs are photographed at a fixed delay after
# their window appears, so anything that moves diverges by construction. This
# re-runs only the top of the ranking with GEX_COMPARE_SELF (gex_wgpu against
# itself, which is animation) and GEX_COMPARE_REF2 (softpipe as a second
# reference, which is where the two references disagree with each other).
#
# Read the result as: rmse means nothing unless it is well above *both* self
# and refdiff. Half the deterministic top of this corpus has previously turned
# out to be llvmpipe rather than the driver.
set -u
ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
IN=${1:?usage: app-compare-top.sh <results.tsv> [count] [outdir]}
COUNT=${2:-20}
OUT=${3:-/tmp/glcompare-top}
LIST=$(mktemp)
# Skip the header, rank by rmse, and resolve each name back to a path the
# comparison harness takes.
awk -F'\t' 'NR>1 && $2 != "" {printf "%s\t%s\n", $2, $1}' "$IN" |
  sort -gr | head -"$COUNT" | cut -f2 |
  while read -r name; do
    p=$(command -v "$name" 2>/dev/null)
    [ -n "$p" ] && echo "$p"
  done > "$LIST"
echo "re-measuring $(wc -l < "$LIST") of the worst $COUNT with self and refdiff"
GEX_COMPARE_SELF=1 GEX_COMPARE_REF2=1 bash "$ROOT/tests/gl-app-compare.sh" \
    "$OUT" "$LIST"
rm -f "$LIST"
