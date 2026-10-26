#!/usr/bin/env python3
"""Fixed preferred bases for 32-bit Wine's i386 Wine DLLs (docs/wine-overhead.md §7).

Every Wine builtin is linked at ImageBase 0x10000000 with DYNAMIC_BASE.
For such an image Wine's server picks the address (server/mapping.c
assign_map_address): one system-wide address per DLL, top-down from a pool
at [0x60000000, 0x7c000000), in the order DLLs are first mapped by any
process, freed when the last mapping closes. So a DLL's base depends on
what every process loaded before it: a winepe.wasm relink that changes the
eagerly loaded set moves the rest, and helper processes' concurrent loads
move them run to run (Noita's msvcp120 at 0x78a10000 one run, 0x78540000
the next, same build). Function-tier units are keyed by image and base, so
a moved DLL silently runs tier-1.

This gives each DLL its own preferred base from a checked-in table
(wine32-bases.txt) and rewrites the DLL's header and relocations to it at stage
time, with DYNAMIC_BASE cleared, so the server leaves it out of its pool
and Wine's loader maps it at that base whenever the range is free, whatever
else loaded first.

Where the slots go: down from 0x7b000000 (above it, 0x7b000000-0x7c000000,
the server's pool keeps 16 MiB for a program's own DYNAMIC_BASE DLLs),
skipping 0x60000000-0x70000000, where GNU ld's --enable-auto-image-base puts
MinGW-built DLLs that games ship at fixed bases (SDL2 0x6c740000, libpng,
zlib, SDL_image...). In the local corpus (1398 i386 PE files, 93 at fixed
bases) the remaining fixed bases are 0x00400000-style programs and
0x10000000 DLLs, both below the table. `check --fixed` reports a game's own
fixed-base images that still collide.

The table is append-only: units built for a slot stay valid as long as the
slot does. `table` updates it in place and never re-packs: a new DLL gets a
free gap (highest first) or a slot below the lowest; a DLL that outgrew
its slot moves alone the same way; a DLL no longer in the dist keeps its
line (and so its range).

  rebase.py table DIR TABLE            add DIR's new or grown DLLs to TABLE (made if missing)
  rebase.py apply DIR TABLE            check, then rebase DIR's DLLs in place
  rebase.py check DIR TABLE [--fixed IMAGE...]
                                       check only (exit 1 on a problem); with
                                       --fixed, also report a program's own
                                       fixed-base images that overlap a slot

DIR is a 32-bit Wine dist's lib/wine/i386-windows (or a 32-bit Wine prefix's
system32/syswow64). The check fails when two slots overlap, when a DLL in
DIR is missing from the table or outgrew its slot, or when a DLL in DIR is
not at its table base with DYNAMIC_BASE cleared (apply not run). A rebased
file is written as a new file (the old one, or a symlink to it, unlinked),
never in place: dists are hard-link copies.

Rebased: 32-bit PE DLLs with base relocations (.dll .drv .sys .ocx .acm .cpl
.ax .ds, an .exe built as a DLL like ntoskrnl.exe, and the native wasm32
skeletons, whose .reloc the loader would apply anyway). Left alone ("keep"
lines): images without relocations (resource-only: tzres, the .tlb files).
Not listed: programs and the 16-bit support modules (.dll16 .drv16 .exe16
.mod16 .vxd), which a Win32 process never maps.
"""
import os
import struct
import sys

TOP = 0x7B000000
AVOID = (0x60000000, 0x70000000)  # GNU ld auto-image-base
FLOOR = 0x20000000
GRANULE = 0x10000
EXTS = {"dll", "drv", "sys", "ocx", "acm", "cpl", "ax", "ds", "msstyles", "tlb"}
HEADER = f"""\
# 32-bit Wine's i386 Wine DLLs: preferred bases (tools/wine-wasm/rebase.py). APPEND-ONLY:
# `rebase.py table DIR wine32-bases.txt` adds new or grown DLLs; never re-pack or
# reorder (each slot keys the function-tier units built for it).
# BASE SLOT NAME (keep: no relocations, left at its linked base)
"""


def align_up(v, a=GRANULE):
    return (v + a - 1) & ~(a - 1)


class PE:
    def __init__(self, path):
        self.path = path
        with open(path, "rb") as f:
            self.b = bytearray(f.read())
        b = self.b
        if b[:2] != b"MZ":
            raise ValueError("not MZ")
        o = struct.unpack_from("<I", b, 0x3C)[0]
        if b[o:o + 4] != b"PE\0\0":
            raise ValueError("not PE")
        self.chars = struct.unpack_from("<H", b, o + 22)[0]
        self.opt = o + 24
        if struct.unpack_from("<H", b, self.opt)[0] != 0x10B:
            raise ValueError("not PE32")
        self.dllchar_at = self.opt + 70
        self.base = struct.unpack_from("<I", b, self.opt + 28)[0]
        self.size = struct.unpack_from("<I", b, self.opt + 56)[0]
        ndirs = struct.unpack_from("<I", b, self.opt + 92)[0]
        self.reloc = struct.unpack_from("<II", b, self.opt + 96 + 5 * 8) if ndirs > 5 else (0, 0)
        nsec = struct.unpack_from("<H", b, o + 6)[0]
        sopt = struct.unpack_from("<H", b, o + 20)[0]
        s = self.opt + sopt
        self.secs = []
        for i in range(nsec):
            vsize, va, rsize, roff = struct.unpack_from("<IIII", b, s + 40 * i + 8)
            self.secs.append((va, max(vsize, rsize), rsize, roff))

    @property
    def is_dll(self):
        return bool(self.chars & 0x2000)

    @property
    def dynamic(self):
        return bool(struct.unpack_from("<H", self.b, self.dllchar_at)[0] & 0x40)

    @property
    def relocatable(self):
        return self.reloc[1] != 0 and not (self.chars & 0x0001)

    def off(self, rva):
        for va, vs, rs, ro in self.secs:
            if va <= rva < va + vs:
                if rva - va + 4 > rs:
                    raise ValueError(f"RVA {rva:#x} past the raw data of its section")
                return ro + rva - va
        raise ValueError(f"RVA {rva:#x} in no section")

    def rebase(self, new):
        """Move to `new`: apply the HIGHLOW relocations, set ImageBase, clear
        DYNAMIC_BASE."""
        delta = (new - self.base) & 0xFFFFFFFF
        b = self.b
        if delta:
            p = self.off(self.reloc[0])
            end = p + self.reloc[1]
            while p + 8 <= end:
                page, bsize = struct.unpack_from("<II", b, p)
                if bsize < 8:
                    break
                for i in range((bsize - 8) // 2):
                    e = struct.unpack_from("<H", b, p + 8 + 2 * i)[0]
                    kind, at = e >> 12, e & 0xFFF
                    if kind == 0:
                        continue
                    if kind != 3:
                        raise ValueError(f"{self.path}: relocation type {kind} at {page + at:#x}")
                    fo = self.off(page + at)
                    v = struct.unpack_from("<I", b, fo)[0]
                    struct.pack_into("<I", b, fo, (v + delta) & 0xFFFFFFFF)
                p += bsize
            struct.pack_into("<I", b, self.opt + 28, new)
            self.base = new
        v = struct.unpack_from("<H", b, self.dllchar_at)[0]
        struct.pack_into("<H", b, self.dllchar_at, v & ~0x40)


def images(d):
    """(name, PE) for the files the table covers, by name."""
    out = []
    for name in sorted(os.listdir(d), key=str.lower):
        ext = name.rsplit(".", 1)[-1].lower() if "." in name else ""
        if ext not in EXTS and ext != "exe":
            continue
        p = os.path.join(d, name)
        if not os.path.isfile(p):
            continue
        try:
            pe = PE(p)
        except (ValueError, struct.error):
            continue
        if pe.is_dll:
            out.append((name, pe))
    return out


def read_table(path):
    """{lower name: [base or None, slot, name]}, in file order."""
    t = {}
    if not os.path.exists(path):
        return t
    with open(path) as f:
        for ln in f:
            ln = ln.split("#", 1)[0].strip()
            if not ln:
                continue
            base, slot, name = ln.split()
            t[name.lower()] = [None if base == "keep" else int(base, 16), int(slot, 16), name]
    return t


def write_table(path, t):
    tmp = path + ".new"
    with open(tmp, "w") as f:
        f.write(HEADER)
        for b, s, n in t.values():
            f.write(f"{'keep' if b is None else f'{b:#010x}'} {s:#09x} {n}\n")
    os.replace(tmp, path)


def place(t, slot):
    """A base for `slot` bytes: the highest free gap under TOP (outside
    AVOID), else just below the lowest slot."""
    used = sorted([(b, b + s) for b, s, _ in t.values() if b is not None] + [AVOID])
    gaps, at = [], FLOOR
    for b, e in used:
        if b > at:
            gaps.append((at, min(b, TOP)))
        at = max(at, e)
    if at < TOP:
        gaps.append((at, TOP))
    for lo, hi in reversed(gaps):
        if hi - lo >= slot:
            return hi - slot
    raise ValueError(f"no room for a {slot:#x} slot")


def update(d, path):
    t = read_table(path)
    added = []
    for name, pe in images(d):
        key = name.lower()
        e = t.get(key)
        if not pe.relocatable:
            if e is None:
                t[key] = [None, pe.size, name]
                added.append(name)
            continue
        slot = align_up(pe.size)
        if e is not None and e[0] is not None and slot <= e[1]:
            continue
        if e is not None:
            del t[key]  # grown (or newly relocatable): moves alone
        t[key] = [place(t, slot), slot, name]
        added.append(name)
    write_table(path, t)
    return added


def problems(d, t, applied):
    bad = []
    rng = sorted((b, b + s, n) for b, s, n in t.values() if b is not None)
    for (b0, e0, n0), (b1, e1, n1) in zip(rng, rng[1:]):
        if b1 < e0:
            bad.append(f"{n0} [{b0:#x},{e0:#x}) overlaps {n1} [{b1:#x},{e1:#x})")
    for b, e, n in rng:
        if b % GRANULE or b < FLOOR or e > TOP:
            bad.append(f"{n}: bad slot [{b:#x},{e:#x})")
    for name, pe in images(d):
        e = t.get(name.lower())
        if e is None:
            bad.append(f"{name}: not in the table (rebase.py table DIR TABLE)")
        elif e[0] is not None and align_up(pe.size) > e[1]:
            bad.append(f"{name}: SizeOfImage {pe.size:#x} outgrew its slot {e[1]:#x} (rebase.py table DIR TABLE)")
        elif e[0] is not None and not pe.relocatable:
            bad.append(f"{name}: has no relocations, cannot move to {e[0]:#x}")
        elif applied and e[0] is not None and (pe.base != e[0] or pe.dynamic):
            bad.append(f"{name}: at {pe.base:#x}{' DYNAMIC_BASE' if pe.dynamic else ''}, the table says {e[0]:#x} (rebase.py apply not run)")
    return bad


def collisions(t, paths):
    """A program's own images that keep their linked base (no DYNAMIC_BASE,
    or no relocations) and overlap a table slot."""
    out = []
    rng = [(b, b + s, n) for b, s, n in t.values() if b is not None]
    for p in paths:
        try:
            pe = PE(p)
        except (OSError, ValueError, struct.error):
            continue
        if pe.dynamic and pe.relocatable:
            continue
        lo, hi = pe.base, pe.base + pe.size
        for b, e, n in rng:
            if lo < e and b < hi:
                out.append(f"{p} at [{lo:#x},{hi:#x}) overlaps {n}'s slot [{b:#x},{e:#x})")
    return out


def main(argv):
    if len(argv) < 4 or argv[1] not in ("table", "apply", "check"):
        sys.stderr.write(__doc__)
        return 2
    cmd, d, path = argv[1], argv[2], argv[3]
    if cmd == "table":
        added = update(d, path)
        print(f"rebase.py: {len(added)} added or moved{': ' + ' '.join(added) if 0 < len(added) <= 20 else ''}")
        return 0
    t = read_table(path)
    if not t:
        print(f"rebase.py: no table {path}", file=sys.stderr)
        return 1
    if cmd == "check":
        fixed = argv[argv.index("--fixed") + 1:] if "--fixed" in argv else []
        bad = problems(d, t, applied=True)
        for m in collisions(t, fixed):
            print(f"rebase.py: collision: {m}", file=sys.stderr)
        for m in bad:
            print(f"rebase.py: {m}", file=sys.stderr)
        return 1 if bad else 0
    bad = problems(d, t, applied=False)
    if bad:
        for m in bad:
            print(f"rebase.py: {m}", file=sys.stderr)
        return 1
    moved = 0
    for name, pe in images(d):
        b = t[name.lower()][0]
        if b is None or (pe.base == b and not pe.dynamic):
            continue
        pe.rebase(b)
        p = os.path.join(d, name)
        mode = os.stat(p).st_mode & 0o7777
        tmp = p + ".rebase"
        with open(tmp, "wb") as f:
            f.write(pe.b)
        os.chmod(tmp, mode)
        os.unlink(p)
        os.rename(tmp, p)
        moved += 1
    print(f"rebase.py: {moved} DLLs rebased, {len(t)} in the table")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
