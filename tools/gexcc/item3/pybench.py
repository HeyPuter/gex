# CPython's zlib module, timed inside the program: compress (level 6) and
# decompress 4 MiB of text-like data, median of 5 after a warm-up. One JSON line.
import zlib, time, sys, json
words = [b"the ", b"gex ", b"wasm ", b"module ", b"table ", b"memory ", b"fork ", b"debian ",
         b"library ", b"function ", b"pointer ", b"page ", b"kernel ", b"process ", b"thread ", b"signal\n"]
x, parts, n = 12345, [], 0
while n < 4 << 20:
    x = (x * 1103515245 + 12345) & 0xffffffff
    w = words[(x >> 16) & 15]
    parts.append(w); n += len(w)
data = b"".join(parts)[: 4 << 20]
tc, td = [], []
for r in range(6):
    t0 = time.perf_counter(); c = zlib.compress(data, 6)
    t1 = time.perf_counter(); d = zlib.decompress(c)
    t2 = time.perf_counter()
    assert d == data
    if r: tc.append((t1 - t0) * 1e3); td.append((t2 - t1) * 1e3)
tc.sort(); td.sort()
print(json.dumps({"variant": sys.argv[1] if len(sys.argv) > 1 else "py", "compress": round(tc[2], 2),
                  "decompress": round(td[2], 2), "clen": len(c), "crc": zlib.crc32(c)}))
