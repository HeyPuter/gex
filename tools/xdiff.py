#!/usr/bin/env python3
"""Align a gex block trace (GEX_TRACE_BLOCKS=1, one line per block entered)
with a native single-step trace (tools/xtrace) of the same program and report
the first block where they diverge.

    tools/xdiff.py gex.txt native.txt [--exe-base 0x1000000] [--interp-base 0x3000000]

Addresses are normalised into (object, offset) so the two address spaces
compare: the native maps come from the `map` lines xtrace prints; gex's are the
fixed bases it places things at (see gex_abi). Stack addresses are compared as
offsets from the initial stack pointer; heap (brk) addresses as offsets from
the executable's end.

The walk: for each gex block, advance the native trace to the next instruction
at the same normalised pc, then compare registers. The first mismatch is where
the guest's state went wrong -- and the native lines just before it are the
instructions that computed the differing value.
"""
import re
import sys

REGS = ["rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi",
        "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15"]
LINE = re.compile(r"(\w+)=(0x[0-9a-fA-F]+|0)(?=\s|$)")


def parse(line):
    d = {}
    for k, v in LINE.findall(line):
        d[k] = int(v, 16) if v != "0" else 0
    return d


class Space:
    """Address normalisation for one address space."""

    def __init__(self):
        self.objs = []  # (name, lo, hi)
        self.stack0 = None

    def add(self, name, lo, hi):
        self.objs.append((name, lo, hi))

    def norm(self, v):
        for name, lo, hi in self.objs:
            if lo <= v < hi:
                return f"{name}+{v - lo:#x}"
        if self.stack0 is not None and self.stack0 - 0x800000 <= v <= self.stack0 + 0x10000:
            return f"S{self.stack0 - v:+#x}"
        return f"{v:#x}"


def load_native(path):
    sp = Space()
    lines = []
    with open(path) as f:
        for line in f:
            if line.startswith("map "):
                # map 00400000-00401000 r--p 00000000 08:01 123 /path
                parts = line.split()
                lo, hi = (int(x, 16) for x in parts[1].split("-"))
                name = parts[-1] if len(parts) >= 7 else "anon"
                base = name.rsplit("/", 1)[-1]
                if "ld-linux" in base or "ld-musl" in base:
                    tag = "L"
                elif base.startswith("[") or base == "anon":
                    # The stack is compared relative to the initial rsp.
                    continue
                else:
                    tag = "E"
                # Merge segments of one object into its span.
                merged = False
                for i, (n, l, h) in enumerate(sp.objs):
                    if n == tag:
                        sp.objs[i] = (n, min(l, lo), max(h, hi))
                        merged = True
                if not merged:
                    sp.add(tag, lo, hi)
            elif line.startswith("pc="):
                lines.append(parse(line))
    if lines:
        sp.stack0 = lines[0]["rsp"]
    return sp, lines


def load_gex(path, exe_base, interp_base):
    sp = Space()
    lines = []
    with open(path) as f:
        for line in f:
            if line.startswith("[trace]"):
                lines.append(parse(line))
    if lines:
        sp.stack0 = lines[0]["rsp"]
    # gex places the interpreter at interp_base and the executable at exe_base;
    # spans are generous, the trace only needs the right object.
    sp.add("L", interp_base, interp_base + 0x00400000)
    sp.add("E", exe_base, interp_base)
    return sp, lines


def main():
    args = sys.argv[1:]
    exe_base = 0x1000000
    interp_base = 0x3000000
    files = []
    i = 0
    while i < len(args):
        if args[i] == "--exe-base":
            exe_base = int(args[i + 1], 16)
            i += 2
        elif args[i] == "--interp-base":
            interp_base = int(args[i + 1], 16)
            i += 2
        else:
            files.append(args[i])
            i += 1
    gsp, g = load_gex(files[0], exe_base, interp_base)
    nsp, n = load_native(files[1])
    print(f"gex: {len(g)} blocks; native: {len(n)} instructions")
    print(f"gex objects: {gsp.objs}  native objects: {nsp.objs}")

    ni = 0
    matched = 0
    for bi, gb in enumerate(g):
        gpc = gsp.norm(gb["pc"])
        # Advance native to the next instruction at this pc.
        j = ni
        while j < len(n) and nsp.norm(n[j]["pc"]) != gpc:
            j += 1
        if j >= len(n):
            print(f"\nblock #{bi} pc {gpc}: never reached natively after native step {ni}")
            print("  gex:   ", fmt(gb, gsp))
            if ni < len(n):
                print("  native continues at:", nsp.norm(n[ni]["pc"]))
                for k in range(max(0, ni - 8), min(len(n), ni + 4)):
                    print("   ", fmt(n[k], nsp))
            return 1
        nb = n[j]
        diffs = []
        for r in REGS:
            if gsp.norm(gb[r]) != nsp.norm(nb[r]):
                diffs.append((r, gsp.norm(gb[r]), nsp.norm(nb[r])))
        if diffs:
            print(f"\nblock #{bi} pc {gpc} (native step {j}): registers differ")
            for r, a, b in diffs:
                print(f"  {r}: gex {a}  native {b}")
            print("  previous gex block:", fmt(g[bi - 1], gsp) if bi else "-")
            print("  native instructions since the previous block:")
            for k in range(max(0, ni), j + 1):
                print("   ", fmt(n[k], nsp))
            return 1
        ni = j + 1
        matched += 1
    print(f"all {matched} blocks match")
    return 0


def fmt(d, sp):
    return f"pc={sp.norm(d['pc'])} " + " ".join(f"{r}={sp.norm(d[r])}" for r in REGS)


if __name__ == "__main__":
    sys.exit(main())
