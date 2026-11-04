#!/usr/bin/env python3
"""pescan.py DIR...: the PE images in each DIR (a dist's lib/wine/ARCH-windows)
that still hold machine code: a code section and neither a .wasm nor a
.wasmref section, so the module would run translated. One line per DIR;
exits 1 when any DIR has one."""
import os
import struct
import sys

CODE = 0x20000020  # IMAGE_SCN_CNT_CODE | IMAGE_SCN_MEM_EXECUTE


def scan(d):
    total, code = 0, []
    for f in sorted(os.listdir(d)):
        p = os.path.join(d, f)
        if not os.path.isfile(p) or f.endswith(('.wasm', '.cwasm')):
            continue
        with open(p, 'rb') as fh:
            b = fh.read(4096)
        if len(b) < 0x40 or b[:2] != b'MZ':
            continue
        pe = struct.unpack_from('<I', b, 0x3c)[0]
        if pe + 24 > len(b) or b[pe:pe + 4] != b'PE\0\0':
            continue
        total += 1
        nsec, opt = struct.unpack_from('<H', b, pe + 6)[0], struct.unpack_from('<H', b, pe + 20)[0]
        secs = []
        for i in range(nsec):
            o = pe + 24 + opt + i * 40
            if o + 40 > len(b):
                break
            secs.append((b[o:o + 8].rstrip(b'\0'), struct.unpack_from('<I', b, o + 36)[0]))
        names = {n for n, _ in secs}
        if b'.wasm' in names or b'.wasmref' in names:
            continue
        if any(c & CODE for _, c in secs):
            code.append(f)
    return total, code


def main(dirs):
    bad = 0
    for d in dirs:
        total, code = scan(d)
        print(f'{os.path.basename(os.path.normpath(d))}: {total} PE files, {len(code)} with code and no .wasmref'
              + (': ' + ' '.join(code) if code else ''))
        bad += len(code)
    return 1 if bad else 0


if __name__ == '__main__':
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    sys.exit(main(sys.argv[1:]))
