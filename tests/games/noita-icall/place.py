#!/usr/bin/env python3
"""Place gex's own indirect-call profile of Noita for a units dir whose images
sit at other bases than the run that counted it.

    place.py UNITS LIST BUILDLOG [--w3 DIST] [--game DIR]

The profile (the *.txt beside this script) is a counting run's `icall/`: one
file per image, named by the image's identity at the base it had then, its
first line naming the page path and that base, then `icall RET TARGET CALLS`
lines with absolute addresses. An image at another base (W3's rebased DLLs,
the game's DLLs Wine places around them) has another identity and other
addresses, so the file as counted would never be found.

LIST is the units list (`PAGEPATH BASE` per line, the order the units were
built in) and BUILDLOG the log of that build: `gexc cgen-image` prints one
`[cgen] indirect-call hints: ... /icall/<identity>.txt` line per image, in
LIST's order, which gives each image's identity at its new base. Every
address inside a profiled image (the return address, and a target in any
profiled image) moves by that image's base delta; a target in no profiled
image is kept. The result goes to UNITS/icall/<identity>.txt; rebuild the
units after (the hints are part of the unit key).
"""
import os, re, struct, sys

here = os.path.dirname(os.path.abspath(__file__))
args = sys.argv[1:]
opts = {"--w3": os.path.expanduser("~/gex-sandbox/scratch/W/page/w3dist"), "--game": os.path.expanduser("~/gex-sandbox/Noita")}
pos = []
i = 0
while i < len(args):
    if args[i] in opts:
        opts[args[i]] = args[i + 1]; i += 2
    else:
        pos.append(args[i]); i += 1
if len(pos) != 3:
    sys.exit(__doc__)
units, listf, logf = pos


def local(page):
    if page.startswith("/w3/"):
        return os.path.join(opts["--w3"], page[4:])
    if page.startswith("/pfx/dosdevices/z:/g/"):
        return os.path.join(opts["--game"], page[len("/pfx/dosdevices/z:/g/"):])
    sys.exit(f"place.py: no local file for {page}")


def size_of_image(path):
    with open(path, "rb") as f:
        b = f.read(4096)
    pe = struct.unpack_from("<I", b, 0x3C)[0]
    return struct.unpack_from("<I", b, pe + 24 + 56)[0]


new = []
for l in open(listf):
    if l.strip():
        p, b = l.split()
        new.append((p, int(b, 16)))
ids = [m.group(1) for m in re.finditer(r"^\[cgen\] indirect-call hints: .*?/icall/([0-9a-f]{64})\.txt", open(logf, errors="replace").read(), re.M)]
if len(ids) != len(new):
    sys.exit(f"place.py: {len(ids)} hint lines in {logf} for {len(new)} images in {listf}: not the log of this list")
newbase = {p: (b, ident) for (p, b), ident in zip(new, ids)}

prof = []  # (page path, old base, size, lines)
for name in sorted(os.listdir(here)):
    if not name.endswith(".txt"):
        continue
    lines = open(os.path.join(here, name)).read().splitlines()
    m = re.match(r"# (\S+) at (0x[0-9a-f]+):", lines[0])
    if not m:
        sys.exit(f"place.py: {name}: no header")
    prof.append((m.group(1), int(m.group(2), 16), size_of_image(local(m.group(1))), lines))

ranges = [(old, old + size, newbase[p][0] - old) for p, old, size, _ in prof if p in newbase]


def move(a):
    for lo, hi, d in ranges:
        if lo <= a < hi:
            return a + d
    return a


os.makedirs(os.path.join(units, "icall"), exist_ok=True)
placed = 0
for p, old, size, lines in prof:
    if p not in newbase:
        print(f"place.py: {p} is not in {listf}: its profile is left out")
        continue
    b, ident = newbase[p]
    out = [f"# {p} at {b:#x}: placed from the profile counted at {old:#x} (tests/games/noita-icall/place.py)"] + lines[1:]
    for k in range(1, len(out)):
        f = out[k].split()
        if len(f) == 4 and f[0] == "icall":
            out[k] = f"icall {move(int(f[1], 16)):#x} {move(int(f[2], 16)):#x} {f[3]}"
    with open(os.path.join(units, "icall", ident + ".txt"), "w") as fh:
        fh.write("\n".join(out) + "\n")
    placed += 1
print(f"place.py: {placed} images' profiles placed in {units}/icall")
