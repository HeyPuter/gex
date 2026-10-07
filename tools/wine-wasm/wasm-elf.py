#!/usr/bin/env python3
"""Wrap a native wasm32 program (a PIE wasm module linked against gex's musl)
in the ELF32 i386 container gexc runs (shims/native/src/wasmprog.rs).

    wasm-elf.py IN.wasm OUT [--guest-i386] [--host-stub BLOB [--runner GEXC]]

Segments:
  R    at BASE:          ELF header, program headers, then the wasm module
  R    next page:        the gex header (GEXWASM\\0 ...) and the two entries,
                         each 16 bytes of hlt (never run: gex binds the
                         module's __gex_start / __gex_thread_start there)
                         With --host-stub (gexcc): the entry jumps to the
                         blob at +0x60 (tools/gexcc/hoststub.c), which an
                         x86 Linux host runs: it re-execs the file under
                         gexc. No PT_GNU_STACK then, so the host maps the
                         read-only page executable (IA-32 READ_IMPLIES_EXEC);
                         gex goes by PF_X alone and runs none of it.
  R X  next page:        hlt (gex wants an executable segment)
  RW   next, aligned:    the module's data (dylink.0 mem_size), zero-filled;
                         __memory_base. brk starts above it.
"""
import struct, sys

BASE = 0x08048000
PAGE = 0x1000


def leb(b, p):
    r = s = 0
    while True:
        x = b[p]; p += 1
        r |= (x & 0x7f) << s; s += 7
        if not x & 0x80:
            return r, p


def dylink_mem(w):
    assert w[:4] == b'\0asm', 'not a wasm module'
    p = 8
    while p < len(w):
        sid = w[p]; p += 1
        n, p = leb(w, p)
        end = p + n
        if sid == 0:
            ln, q = leb(w, p)
            name = w[q:q + ln]; q += ln
            if name == b'dylink.0':
                while q < end:
                    kind = w[q]; q += 1
                    sl, q = leb(w, q)
                    if kind == 1:
                        size, q2 = leb(w, q)
                        align, _ = leb(w, q2)
                        return size, align
                    q += sl
        p = end
    raise SystemExit('no dylink.0 memory info: link with --experimental-pic -pie')


def up(v, a):
    return (v + a - 1) & ~(a - 1)


def main():
    argl = sys.argv[1:]
    stub, runner = None, ''
    for opt in ('--host-stub', '--runner'):
        if opt in argl:
            k = argl.index(opt)
            v = argl[k + 1]
            del argl[k:k + 2]
            if opt == '--host-stub':
                stub = open(v, 'rb').read()
            else:
                runner = v
    args = [a for a in argl if not a.startswith('--')]
    src, dst = args[0], args[1]
    # --guest-i386: the program runs i386 code (Wine's loader, W3): gex
    # translates i386 in its process and leaves Windows' addresses free.
    flags = 1 if '--guest-i386' in sys.argv[1:] else 0
    w = open(src, 'rb').read()
    mem_size, mem_align = dylink_mem(w)
    ehsize, phsize, nph = 52, 32, (4 if stub else 5)
    wasm_off = up(ehsize + phsize * nph, 16)
    r_end = wasm_off + len(w)
    text_off = up(r_end, PAGE)
    text_va = BASE + text_off
    entry = text_va + 0x40
    code_va = text_va + PAGE
    data_va = up(code_va + PAGE, max(PAGE, 1 << mem_align))
    data_sz = up(max(mem_size, 16), PAGE)

    hdr = b'GEXWASM\0' + struct.pack('<8I', 1, BASE + wasm_off, len(w), data_va, data_sz, entry, entry + 16, flags)
    text = hdr.ljust(0x40, b'\0') + b'\xf4' * 32
    if stub:
        mark = stub.find(b'GEXCC-RUNNER')
        assert mark >= 0, 'host stub without its runner slot'
        rb = runner.encode()
        assert len(rb) < 256, 'runner path too long'
        stub = stub[:mark] + rb.ljust(256, b'\0') + stub[mark + 256:]
        # entry: jmp rel32 to the stub at +0x60
        text = text[:0x40] + b'\xe9' + struct.pack('<i', 0x60 - 0x45) + text[0x45:]
        text = text[:0x60] + stub
        assert len(text) <= PAGE, 'host stub too big'
    text = text.ljust(PAGE, b'\xf4')

    eh = b'\x7fELF' + bytes([1, 1, 1, 0]) + bytes(8)
    eh += struct.pack('<HHIIIIIHHHHHH', 2, 3, 1, entry, ehsize, 0, 0, ehsize, phsize, nph, 40, 0, 0)
    ph = b''
    # PT_LOAD R: headers + wasm
    ph += struct.pack('<8I', 1, 0, BASE, BASE, r_end, r_end, 4, PAGE)
    # PT_LOAD R: gex header + entries. Not executable, so gex translates
    # nothing there; the first call to an entry misses and finds the
    # function table's binding.
    ph += struct.pack('<8I', 1, text_off, text_va, text_va, PAGE, PAGE, 4, PAGE)
    # PT_LOAD R X: one page of hlt. gex refuses an image with nothing
    # executable; nothing jumps here.
    ph += struct.pack('<8I', 1, text_off + PAGE, code_va, code_va, PAGE, PAGE, 5, PAGE)
    # PT_LOAD RW: data (bss only)
    ph += struct.pack('<8I', 1, text_off + 2 * PAGE, data_va, data_va, 0, data_sz, 6, PAGE)
    if not stub:
        # PT_GNU_STACK RW
        ph += struct.pack('<8I', 0x6474e551, 0, 0, 0, 0, 0, 6, 16)
    out = (eh + ph).ljust(wasm_off, b'\0') + w
    out = out.ljust(text_off, b'\0') + text + b'\xf4' * PAGE
    open(dst, 'wb').write(out)
    import os
    os.chmod(dst, 0o755)


main()
