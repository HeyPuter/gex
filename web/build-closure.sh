#!/bin/sh
# Statically recompile a program and everything it reaches, for the site.
#
# This is the configuration the project exists for: the guest arrives already
# translated and lifts almost nothing at run time. Until now only the harness
# page could be given a closure, so the page anyone actually opens had only
# ever run on the runtime translator -- the secondary path -- and the two
# disagreed for days about whether `apt-get update` worked at all.
#
#   web/build-closure.sh debian /tmp/gex-debroot /usr/bin/apt-get
#
# writes `web/public/closure/<name>/` with one module per object, plus the
# manifest `web/src/closure.js` reads.
#
# *Not* `--exhaustive`. That flag translates every decodable address because a
# built module has no translator behind it -- which is true of the native host
# and false here, where the seed translator is the miss path and anything the
# closure does not cover is lifted on demand. Exhaustive, apt's closure is
# 678 MB and libgcrypt alone is 318 MB of code nothing in an `apt-get update`
# ever enters. Reachable, the whole thing is 82 MB.
set -e
cd "$(dirname "$0")/.."

name=${1:?usage: build-closure.sh <name> <sysroot> <program>}
sysroot=${2:?usage: build-closure.sh <name> <sysroot> <program>}
program=${3:?usage: build-closure.sh <name> <sysroot> <program>}

out="web/public/closure/$name"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cargo build --release -p gexc 2>&1 | grep -E "^error" && exit 1
echo "building the closure for $program..."
target/release/gexc build --closure --sysroot "$sysroot" --no-shared-memory \
    "$sysroot$program" -o "$work/c.wasm"

mkdir -p "$out"
rm -f "$out"/*.wasm "$out"/closure.json
cp "$work"/*.wasm "$out/"

# The manifest, as `loadClosure` wants it: the image module's name, the
# libraries in placement order, and where each object was pinned.
#
# The pins are hex -- `0x5000000 0x81000 /usr/lib/...` -- and reading them as
# decimal gives zero for every line. That does not fail: it produces a
# manifest with no layout at all, the libraries land wherever the loader puts
# them, and every block in them has its address baked in. A closure that loads
# and then runs the wrong code is worse than one that does not load.
python3 - "$work" "$out" <<'PY'
import json, os, sys
work, out = sys.argv[1], sys.argv[2]
libs = sorted(n for n in os.listdir(out)
              if n.startswith('c.') and n.endswith('.wasm') and n != 'c.wasm')
pins = {}
for line in open(os.path.join(work, 'c.pins')):
    f = line.split()
    if len(f) < 3:
        continue
    at = int(f[0], 0)
    if at > 0:
        pins[' '.join(f[2:])] = {'base': at, 'span': int(f[1], 0)}
json.dump({'image': 'c.wasm', 'libs': libs, 'pins': pins},
          open(os.path.join(out, 'closure.json'), 'w'))
print(f"  {len(libs)} libraries, {len(pins)} pins")
PY

echo "web/public/closure/$name: $(du -sh "$out" | cut -f1)"
