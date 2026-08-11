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

name=${1:?usage: build-closure.sh <name> <sysroot> <program> [more...]}
sysroot=${2:?usage: build-closure.sh <name> <sysroot> <program> [more...]}
program=${3:?usage: build-closure.sh <name> <sysroot> <program> [more...]}
shift 3

# Everything after the program is another root of the same closure.
#
# Only the first program's *image* becomes a module -- two images would both
# want `PIE_BASE` and the second's dispatch entries would land on the first's
# pages -- but every root's libraries do. So the first argument should be the
# program the page runs as pid 1, and the rest are what it goes on to exec.
#
# This is what the demo was missing. Its closure was built for `apt-get` while
# pid 1 is `bash`, so bash was the one program in the run with no module: 18,595
# blocks in pid 1 and another 4,106 in every child it forks, which by then was
# *all* of what either of them lifted. A directory of helpers goes in whole --
# `/usr/lib/apt/methods` is apt's.
withs=""
for w in "$@"; do
    if [ -d "$sysroot$w" ]; then
        withs="$withs --with-dir $sysroot$w"
    else
        withs="$withs --with $sysroot$w"
    fi
done

out="web/public/closure/$name"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cargo build --release -p gexc 2>&1 | grep -E "^error" && exit 1
echo "building the closure for $program..."
# The dynamic linker whole; everything else reachable-only.
#
# *Not* libc, and this is a correctness bound rather than a size one. Built
# whole, libc breaks `apt-get update`: children die at whatever syscall they
# are in, and bisecting the closure with `?libs=N` puts it on libc alone --
# `?libs=2` runtime-translates libc and crashes nothing, `?libs=3` adds the
# closure's libc and three children die. Reachable-only libc, same closure,
# same command: zero crashes and the run passes. The linker stays whole in
# that build, so it is not whole-object translation in general.
#
# Whole is what every program in a rootfs wants from libc, so this costs
# real coverage and should come back when the exhaustive path is fixed.
#
# "Reachable" means reachable *from this program*, so a closure built for
# `apt-get` holds only the libc paths apt walks -- and `/bin/true` against it
# lifts 9,800 blocks where against a closure built for itself it lifts none.
# libc and ld.so are what every program in a rootfs shares, and a demo runs
# dozens of programs from one closure.
target/release/gexc build --closure --sysroot "$sysroot" --no-shared-memory \
    --exhaustive-for "${GEX_WHOLE:-ld-linux}" \
    --shared-image $withs "$sysroot$program" -o "$work/c.wasm"

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
python3 - "$work" "$out" "$program" <<'PY'
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
# The program the image module was built from.
#
# `c.wasm` is one program's own text at `PIE_BASE`, and handing it to a page
# whose pid 1 is a different program starts the guest inside somebody else's
# code -- it exits without making a syscall, which is exactly what the first
# attempt at this did. Named here so the loader can check rather than trust.
# No image module: `--shared-image` emits the program's own text as an
# ordinary library, so it is registered for its pages and instantiated by the
# first branch that lands in one, like everything else. The seed stays the one
# module that owns the memory and the tables.
#
# What that buys is the forked child that immediately `execve`s something
# else. It used to instantiate the image module because the image module *was*
# the guest module -- 42 MB and 212,000 functions of `bash` in every one of
# the hundred processes an `apt-get update` starts, none of which run a shell.
json.dump({'image': None, 'program': sys.argv[3],
           'libs': libs, 'pins': pins},
          open(os.path.join(out, 'closure.json'), 'w'))
print(f"  {len(libs)} libraries, {len(pins)} pins")
PY

echo "web/public/closure/$name: $(du -sh "$out" | cut -f1)"
