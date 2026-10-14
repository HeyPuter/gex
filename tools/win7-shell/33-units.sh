#!/bin/bash
# 33: the function-tier units, $W7_OUT/units (a gexc cgen cache: KEY/c.wasm,
# KEY/c.entries), and the session the eject takes, $W7_OUT/rec/w3-session.json.
# Each "GUEST_PATH at base ADDRESS" of units.txt (32) is built from the
# bundle's tree with `gexc cgen-image --pe-base`. The session is the
# committed recording (w3-session.json: what the shell's run started,
# loaded and read, recorded with tools/eject/record-w3.mjs; it holds names
# and paths only) with its units' keys replaced by this build's and its
# bundle pointed at shell.tar. A unit's key names the image's bytes and
# base, so a new dist or prefix needs new units; the page runs an image
# with no unit through its other tiers.
. "$(dirname "$0")/lib.sh"
G=$GEX_SRC/target/x86/release/gexc; B=$W7_OUT/bundle; U=$W7_OUT/units; L=$W7_OUT/logs
[ -f "$W7_OUT/units.txt" ] && [ -d "$B/pfx" ] || { echo "run 32-bundle.sh first" >&2; exit 2; }
mkdir -p "$U" "$W7_OUT/rec"; : > "$L/units.log"; : > "$W7_OUT/unit-keys.txt"
n=$(wc -l < "$W7_OUT/units.txt"); i=0
step "$n units (logs/units.log)"
while IFS= read -r line; do
  p=${line% at base *} base=${line##* at base }; i=$((i + 1))
  h=$(realpath -m "$B$p")
  [ -f "$h" ] || { echo "units: $p is not in the tree" >&2; exit 1; }
  # the key: the unit's directory in the cache, as cgen-image reports it
  # (cgen-image ends with "IMAGE at BASE: CACHE/KEY/c.wasm (Ns)")
  log=$(GEX_CGEN_CACHE=$U nice -n 10 "$G" cgen-image "$h" --pe-base "$base" --jobs "$JOBS" 2>&1 | tee -a "$L/units.log")
  # an image with no code (normaliz.dll: forwards only) has no unit
  if echo "$log" | grep -q '^\[cgen\] recovered 0 functions'; then echo "  [$i/$n] $(basename "$p"): no code, no unit"; continue; fi
  out=$(echo "$log" | grep " at $base: " | tail -1)
  key=$(echo "$out" | sed -n "s|.* at $base: .*/\([0-9a-f]*\)/c\.wasm ([0-9.]*s)\$|\1|p")
  [ ${#key} = 64 ] && [ -f "$U/$key/c.wasm" ] || { echo "units: no unit for $p at $base (logs/units.log)" >&2; exit 1; }
  echo "$key $p" >> "$W7_OUT/unit-keys.txt"
  echo "  [$i/$n] $(basename "$p") at $base: $key"
done < "$W7_OUT/units.txt"
step "the session"
python3 -I - "$W7_TOOLS/w3-session.json" "$W7_OUT/unit-keys.txt" "$W7_OUT/shell.tar" "$W7_OUT/rec/w3-session.json" <<'PY'
import json, sys
src, keys, tar, out = sys.argv[1:]
v = json.load(open(src))
built = [l.split(' ', 1) for l in open(keys).read().splitlines()]
v['units'] = [{'key': k, 'path': p} for k, p in built]
v['bundle'] = tar
json.dump(v, open(out, 'w'), indent=1)
print(f'{len(built)} units')
PY
step "done: $U, $W7_OUT/rec/w3-session.json"
