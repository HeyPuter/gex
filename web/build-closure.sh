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
# libc, the dynamic linker and the program itself whole; the rest
# reachable-only.
#
# The program is in the list because it is pid 1 and every run pays for it.
# Left out by hand once, `bash` came back reachable-only at 50,961 blocks
# against 197,853, and pid 1 went from lifting 7 blocks to lifting 13,851 --
# which read like a regression in the translator and was a missing word in a
# build command. Derived from `$program` now rather than remembered.
#
# libc whole was briefly impossible: it broke `apt-get update`, whose children
# died at whatever syscall they were in. Bisecting with `?libs=N` put it on
# libc alone -- `?libs=2` runtime-translates libc and crashes nothing,
# `?libs=3` adds the closure's libc and three children die -- and the cause
# was exhaustive discovery seeding every decodable address in a `PF_X`
# mapping, which for a shared library holds the string tables and `.rodata`
# as well as the code. It asks `is_executable` now, libc whole loses the
# 13,215 blocks that were never instructions, and the run passes.
#
# "Reachable" means reachable *from this program*, so a closure built for
# `apt-get` holds only the libc paths apt walks -- and `/bin/true` against it
# lifts 9,800 blocks where against a closure built for itself it lifts none.
# libc and ld.so are what every program in a rootfs shares, and a demo runs
# dozens of programs from one closure.
#
# `GEX_WHOLE=all` translates every object whole. Reachable-only discovery
# cannot follow a virtual call, so a C++ library comes out mostly missing:
# apt-get lifts 78,938 blocks of libapt-pkg at run time -- 1.9 seconds --
# from a closure that contains libapt-pkg, because almost everything in it
# is reached through a vtable. The whole build is larger and nothing else
# changes; the page fetches a library only when a guest branches into it.
whole="${GEX_WHOLE:-libc.so,ld-linux,$(basename "$program")}"
if [ "$whole" = all ]; then
    mode="--exhaustive"
else
    mode="--exhaustive-for $whole"
fi
target/release/gexc build --closure --sysroot "$sysroot" --no-shared-memory \
    $mode --shared-image $withs "$sysroot$program" -o "$work/c.wasm"

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
# Which library holds which program's image.
#
# Every root of the closure is an image now, all of them at `PIE_BASE`, and
# the runtime hands a process the pages of the one whose program it is
# actually running. Without this map it could not tell them apart, and `ls`
# would get `bash`'s blocks -- which is the failure that kept image objects
# down to one per closure in the first place.
# The `gex.meta` section of each module, written out beside it.
#
# Registering a library needs only its metadata -- which pages it covers and
# how far into the slot space it reaches -- and that is a few kilobytes. The
# module itself is wanted by the branch that lands in it. Until this existed
# the page had no way to get one without the other, so it fetched all 239 MB
# of a closure and compiled it before pid 1 ran an instruction, and the load
# was longer than the run.
def section(path):
    b = open(path, 'rb').read()
    at = 8
    while at < len(b):
        sid = b[at]; at += 1
        n = 0; sh = 0
        while True:
            byte = b[at]; at += 1
            n |= (byte & 0x7f) << sh; sh += 7
            if not byte & 0x80:
                break
        end = at + n
        if sid == 0:
            p, ln, sh = at, 0, 0
            while True:
                byte = b[p]; p += 1
                ln |= (byte & 0x7f) << sh; sh += 7
                if not byte & 0x80:
                    break
            if b[p:p + ln] == b'gex.meta':
                return b[p + ln:end]
        at = end
    return b''

metas = []
for n in libs:
    d = section(os.path.join(out, n))
    m = n[:-len('.wasm')] + '.meta'
    open(os.path.join(out, m), 'wb').write(d)
    metas.append(m if d else '')

images = {}
ipath = os.path.join(work, 'c.images')
if os.path.exists(ipath):
    for line in open(ipath):
        f = line.split()
        if len(f) >= 2:
            images[f[0]] = ' '.join(f[1:])
json.dump({'image': None, 'program': sys.argv[3], 'images': images,
           'libs': libs, 'metas': metas, 'pins': pins},
          open(os.path.join(out, 'closure.json'), 'w'))
print(f"  {len(libs)} libraries, {len(pins)} pins, {len(images)} images")
PY

echo "web/public/closure/$name: $(du -sh "$out" | cut -f1)"
