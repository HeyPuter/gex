#!/bin/sh
# Re-run the most divergent rows of a gl-app-compare run with every control
# column switched on.
#
#   tests/gates/compare-followup.sh [indir] [count] [outdir]
#
# tests/gl-app-compare.sh photographs 341 programs against llvmpipe and ranks
# them by RMSE. That first pass is deliberately cheap -- one extra run per
# program for the animation floor -- because three extra runs over the whole
# corpus costs most of a day. But a bare RMSE is not a verdict, and its own
# header says so: a number means nothing unless it is well above **all** of
#
#   self     the same driver twice: the animation floor. Anything that moves
#            between frames diverges from itself by construction.
#   refdiff  llvmpipe against softpipe: whether the *references* agree. They
#            disagree by 0.251 on `quad`, more than this driver disagrees with
#            either, because llvmpipe splits an immediate-mode quad on the
#            opposite diagonal.
#   load     llvmpipe against itself pinned to one core: whether the picture
#            encodes how much *work finished*. `gltestperf` scored 0.306 with
#            self 0 -- deterministic, and every existing column called it a
#            real defect -- while llvmpipe pinned disagrees with itself by
#            0.356.
#
# So the workflow is two passes, and this is the second: take the top of the
# first pass's ranking and spend the three extra runs on those alone. Twenty
# programs is a few minutes; the whole corpus would be hours.
set -u
IN=${1:-/tmp/glcompare-0914}
COUNT=${2:-20}
OUT=${3:-${IN}-deep}
LIST=${GEX_COMPARE_LIST:-/tmp/glapps.txt}

[ -s "$IN/results.tsv" ] || {
    echo "no results at $IN/results.tsv -- run tests/gl-app-compare.sh first" >&2
    exit 2
}

. "$(dirname "$0")/gpu-exclusive.sh"
gex_require_idle_gpu "compare-followup" || exit 2

# The ranking holds *names*; gl-app-compare.sh wants the paths its list holds.
# Matching on the basename rather than reconstructing /usr/bin/<name>: the list
# is built by scanning for binaries that link libGL and nothing promises they
# all live in one directory.
cand=$(mktemp)
# Ranked by how far each row stands **above its own animation floor**, not by
# raw RMSE. The first pass already measures `self` -- the same driver twice --
# and on this corpus the largest raw numbers are mostly animated programs
# diverging from themselves just as much: `clear-repeat` scored rmse 0.254
# against self 0.549, and would have taken one of these slots while telling us
# nothing. A deterministic row (self 0) keeps its full score, which is what
# puts the real candidates at the top.
#
# Rows whose self is missing ("-") are treated as a zero floor: unmeasured is
# not the same as zero, but ranking them as if animated would hide a row that
# was never given the control run.
awk -F'\t' 'NR>1 && $2 != "-" {
       floor = ($3 == "-" || $3 == "") ? 0 : $3
       above = $2 - floor
       if (above > 0) printf "%.9f\t%s\n", above, $1
     }' "$IN/results.tsv" \
    | sort -gr | head -n "$COUNT" | cut -f2 > "$cand.names"
while read -r name; do
    grep -m1 -E "/${name}\$" "$LIST" || echo "  (not in $LIST): $name" >&2
done < "$cand.names" > "$cand"

n=$(grep -c . "$cand" 2>/dev/null || echo 0)
echo "following up on $n of the $COUNT most divergent rows in $IN"
echo

GEX_COMPARE_SELF=1 GEX_COMPARE_REF2=1 GEX_COMPARE_LOAD=1 \
    bash "$(dirname "$0")/../gl-app-compare.sh" "$OUT" "$cand"

echo
echo "---- read this as: a row is a candidate defect only where rmse is well"
echo "     above self, refdiff AND load. Any one of them close to rmse"
echo "     explains the number without the driver being wrong. ----"
rm -f "$cand" "$cand.names"
