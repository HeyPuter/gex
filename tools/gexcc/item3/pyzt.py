# item 3's real-program test: CPython's zlib module (which allocates through
# its own zalloc/zfree, PyMem_RawMalloc: zlib calls back into the guest) on
# Debian's libz.so.1, translated or replaced by the LP64 wasm build. Prints
# one line per check and a digest of everything; both must agree.
import zlib, hashlib, os

h = hashlib.sha256()
def out(name, v):
    h.update(repr((name, v)).encode())
    print(name, v if not isinstance(v, bytes) else (len(v), hashlib.sha256(v).hexdigest()[:16]))

words = [b"the ", b"gex ", b"wasm ", b"module ", b"zlib ", b"fork ", b"page\n"]
x = 12345
buf = bytearray()
while len(buf) < 1 << 20:
    x = (x * 1103515245 + 12345) & 0xffffffff
    buf += words[(x >> 16) % len(words)]
data = bytes(buf)

out("version", zlib.ZLIB_RUNTIME_VERSION)
out("crc32", zlib.crc32(data))
out("adler32", zlib.adler32(data))
for lvl in (0, 1, 6, 9):
    c = zlib.compress(data, lvl)
    assert zlib.decompress(c) == data
    out(f"compress{lvl}", c)

# streaming, with copies of open streams
co = zlib.compressobj(6, zlib.DEFLATED, -15, 9, zlib.Z_FILTERED)
mid = len(data) // 2
parts = [co.compress(data[i:min(i + 65536, mid)]) for i in range(0, mid, 65536)]
cc = co.copy()
rest = [co.compress(data[i:i + 65536]) for i in range(mid, len(data), 65536)]
a = b"".join(parts + rest) + co.flush()
b = b"".join(parts) + cc.compress(data[mid:]) + cc.flush()
out("stream", a)
out("stream-copy", b)
do = zlib.decompressobj(-15)
half = do.decompress(a[:len(a) // 2])
dc = do.copy()
assert half + do.decompress(a[len(a) // 2:]) + do.flush() == data
assert half + dc.decompress(a[len(a) // 2:]) + dc.flush() == data

# a preset dictionary
zd = b"gex wasm module zlib fork page the "
co = zlib.compressobj(zdict=zd)
c = co.compress(data[:4096]) + co.flush()
assert zlib.decompressobj(zdict=zd).decompress(c) == data[:4096]
out("zdict", c)

# the gzip format (wbits 31)
co = zlib.compressobj(9, zlib.DEFLATED, 31)
g = co.compress(data) + co.flush()
assert zlib.decompress(g, 47) == data
out("gzip", g)

# fork with an open stream: both finish it
co = zlib.compressobj(6)
head = co.compress(data[: len(data) // 2])
r, w = os.pipe()
pid = os.fork()
tail = co.compress(data[len(data) // 2:]) + co.flush()
whole = head + tail
ok = zlib.decompress(whole) == data
if pid == 0:
    os.write(w, hashlib.sha256(whole).digest() + bytes([ok]))
    os._exit(0)
child = os.read(r, 33)
_, st = os.waitpid(pid, 0)
out("fork", (child[:32] == hashlib.sha256(whole).digest(), child[32] == 1, ok, st))
out("forked-stream", whole)

print("digest", h.hexdigest())
