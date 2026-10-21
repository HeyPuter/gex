#!/bin/bash
# Page leg: a program exec'd from a fork of a large parent, and a fresh one
# started after it, must not translate and install far more than a warm
# fresh start. Counts, not time, so the leg holds on a loaded machine.
#
# In the page (web/test/site.mjs native-debian-run, Debian arm64 node from
# apt over Wisp):
#   node -e 0                        cold: fills the caches
#   node -e 0                        A: a warm fresh start, the reference
#   node -e 'spawnSync(node -e 1)'   C: the child, exec'd from a fork of node
#   node -e 0                        B: a warm fresh start after it
# and from each process's exit line ("[gex] pid N: lived ..."), modules
# installed and why[nofill=]. Passes when C installs at most 2x what A does
# and B at most 1.5x, and B's nofill is at most 3x A's (+500).
#
# What it catches: e4779cca marked every install dead at exec, so thread
# Workers of an exec'd program skipped unit installs it reused and lifted
# block by block -- in the page, children installed 37555 modules (12134
# before) and a later node -e 0 42829 (9119 before) with nofill 31119 (1596
# before). run-node does not show it (its counts were the same before and
# after), hence a page leg.
#
#   tests/gexcc/exec-reuse-page.sh        (from the worktree root, on edger
#                                          under scratch/bench/bench.sh;
#                                          web/dist built)
# Env: WISP (default wss://wisp.mercurywork.shop/), OUT (the run's log).
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
OUT=${OUT:-$(mktemp)}
read -r -d '' CMD <<'CMDEOF'
(apt-get update -q; apt-get install -y -q nodejs) >/tmp/apt.log 2>&1; echo apt rc=$?
node -e 0; echo cold rc=$?
node -e 0; echo A rc=$?
node -e "const r=require('child_process').spawnSync('node',['-e','1']); console.log('C rc='+r.status)"
node -e 0; echo B rc=$?
echo RUN=$((1+1))
CMDEOF
cd "$ROOT" || exit 2
GEX_QUERY="wisp=${WISP:-wss://wisp.mercurywork.shop/}" GEX_WALL=3000 GEX_STDERR=all GEX_TAIL=0 GEX_CMD="$CMD" \
  node web/test/site.mjs native-debian-run > "$OUT" 2>&1
site=$?
for k in apt cold A C B; do
  grep -aq "| $k rc=0" "$OUT" || { echo "exec-reuse: FAIL: '$k' did not exit 0 (site rc=$site, log $OUT)"; exit 1; }
done
# One exit line per pid (the page prints each through two consoles).
grep -a 'pid [0-9]*: lived' "$OUT" | grep -av '^ *!' | sed -E 's/.*\[gex\] (pid [0-9]+: lived)/\1/' | awk '!seen[$2]++' > "$OUT.lived"
awk -v out="$OUT" '
  /\{node -e 0\}/ { n0++; if (n0 == 2) { ai = inst(); an = nofill() } if (n0 == 3) { bi = inst(); bn = nofill() } }
  /\{node -e 1\}/ { ci = inst(); cn = nofill() }
  function inst(   s) { s = $0; sub(/.* installing /, "", s); return s + 0 }
  function nofill(   s) { s = $0; if (!sub(/.*nofill=/, "", s)) return 0; return s + 0 }
  END {
    if (n0 < 3 || ci == "") { print "exec-reuse: FAIL: missing exit lines (" n0 " node -e 0, child " (ci == "" ? "none" : "seen") "), log " out; exit 1 }
    printf "exec-reuse: A (warm fresh) installs %d nofill %d; C (exec child) %d nofill %d; B (fresh after) %d nofill %d\n", ai, an, ci, cn, bi, bn
    bad = 0
    if (ci > 2 * ai) { print "exec-reuse: FAIL: the exec-ed child installs more than 2x a fresh start"; bad = 1 }
    if (bi > 1.5 * ai) { print "exec-reuse: FAIL: a fresh start after it installs more than 1.5x"; bad = 1 }
    if (bn > 3 * an + 500) { print "exec-reuse: FAIL: a fresh start after it misses unfilled more than 3x"; bad = 1 }
    if (!bad) print "exec-reuse: PASS"
    exit bad
  }' "$OUT.lived"
