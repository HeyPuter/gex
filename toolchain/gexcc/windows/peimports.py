#!/usr/bin/env python3
"""peimports.py SKELETON UNDEFINED [OUT]: give a native module's PE32+
skeleton (winebuild --fake-module --wasm-shared) an import table naming
the DLL functions its module calls (pelink's DLL.wasm.undefined lines
"dll!name"), so Wine's loader loads and initialises those DLLs first, as
it would a Windows program's."""
import struct, sys

skel, undef = sys.argv[1], sys.argv[2]
out = sys.argv[3] if len(sys.argv) > 3 else skel
b = bytearray(open(skel, 'rb').read())

imports = {}
for line in open(undef):
    line = line.strip()
    if '!' not in line:
        continue
    dll, name = line.split('!', 1)
    imports.setdefault(dll.lower(), []).append(name)

e = struct.unpack_from('<I', b, 0x3c)[0]
nsec = struct.unpack_from('<H', b, e + 6)[0]
optsz = struct.unpack_from('<H', b, e + 20)[0]
# (winebuild can leave an empty section header last: its slot is reused)
while nsec and not any(b[e + 24 + optsz + 40 * (nsec - 1):e + 24 + optsz + 40 * nsec]):
    nsec -= 1
opt = e + 24
assert struct.unpack_from('<H', b, opt)[0] == 0x20b, 'not PE32+'
sec_align, file_align = struct.unpack_from('<II', b, opt + 32)
hdr_size = struct.unpack_from('<I', b, opt + 60)[0]
sects = opt + optsz
dd = opt + 112  # data directories (PE32+)

# room for one more section header: grow the headers by a file alignment
# unit when there is none (RVAs stay; only file offsets move)
# (winebuild's SizeOfHeaders can be short of its section table: the first
# section's data is where the headers end)
raws = [struct.unpack_from('<I', b, sects + 40 * i + 20)[0] for i in range(nsec)]
hdr_size = max(hdr_size, min([r for r in raws if r] or [hdr_size]))
end_hdr = sects + 40 * (nsec + 1)
if end_hdr > hdr_size:
    grow = (end_hdr - hdr_size + file_align - 1) // file_align * file_align
    b[hdr_size:hdr_size] = bytes(grow)
    for i in range(nsec):
        p = sects + 40 * i + 20
        raw = struct.unpack_from('<I', b, p)[0]
        if raw:
            struct.pack_into('<I', b, p, raw + grow)
    hdr_size += grow
    struct.pack_into('<I', b, opt + 60, hdr_size)

end = 0
for i in range(nsec):
    vsize, vaddr, rsize, raw = struct.unpack_from('<IIII', b, sects + 40 * i + 8)
    end = max(end, vaddr + max(vsize, rsize))
rva = (end + sec_align - 1) // sec_align * sec_align
fpos = (len(b) + file_align - 1) // file_align * file_align

# layout: descriptors, then per DLL its lookup table, address table,
# hint/name entries and name
dlls = sorted(imports)
desc_size = 20 * (len(dlls) + 1)
data = bytearray(desc_size)
descs = []
for dll in dlls:
    names = imports[dll]
    ilt = len(data); data += bytes(8 * (len(names) + 1))
    iat = len(data); data += bytes(8 * (len(names) + 1))
    for i, n in enumerate(names):
        if len(data) & 1:
            data += b'\0'
        hn = len(data)
        data += struct.pack('<H', 0) + n.encode() + b'\0'
        struct.pack_into('<Q', data, ilt + 8 * i, rva + hn)
        struct.pack_into('<Q', data, iat + 8 * i, rva + hn)
    nm = len(data); data += dll.encode() + b'\0'
    descs.append((ilt, nm, iat))
for i, (ilt, nm, iat) in enumerate(descs):
    struct.pack_into('<IIIII', data, 20 * i, rva + ilt, 0, 0, rva + nm, rva + iat)
vsz = len(data)
data += bytes((-len(data)) % file_align)

b += bytes(fpos - len(b))
b += data
struct.pack_into('<8sIIIIIIHHI', b, sects + 40 * nsec, b'.idata', vsz, rva, len(data), fpos,
                 0, 0, 0, 0, 0xc0000040)  # CNT_INITIALIZED_DATA|MEM_READ|MEM_WRITE
struct.pack_into('<H', b, e + 6, nsec + 1)
struct.pack_into('<I', b, opt + 56, (rva + vsz + sec_align - 1) // sec_align * sec_align)  # SizeOfImage
struct.pack_into('<II', b, dd + 8, rva, desc_size)  # IMAGE_DIRECTORY_ENTRY_IMPORT
open(out, 'wb').write(b)
print(f'{out}: {sum(len(v) for v in imports.values())} imports from {len(dlls)} DLLs ({", ".join(dlls)})')
