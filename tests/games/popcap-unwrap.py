#!/usr/bin/env python3
"""Extract the game executable from a PopCap DRM wrapper (docs/games.md).

    popcap-unwrap.py WRAPPER.exe OUT.exe

A PopCap DRM wrapper (Peggle.exe) carries the real game after its PE
sections, between "!popcapdrmprotect!" (plus a header) and
"!popcapdrmprotend!". At run time it writes that out as popcapgame1.exe and
starts it as a child process; this writes the same file, byte for byte, so
the game runs as the process itself. (After stream S5's loader check and
peggle-wasm's port/unwrap.py.)
"""
import struct
import sys


def unwrap(data):
    pe = struct.unpack_from('<I', data, 0x3c)[0]
    count = struct.unpack_from('<H', data, pe + 6)[0]
    optional = struct.unpack_from('<H', data, pe + 20)[0]
    # The end of the last section's raw data is where the overlay starts.
    end = max(sum(struct.unpack_from('<II', data, pe + 24 + optional + i * 40 + 16)) for i in range(count))
    if not data.startswith(b'!popcapdrmprotect!', end):
        raise SystemExit('no PopCap DRM payload after the sections')
    start = data.index(b'MZ', end)
    stop = data.index(b'!popcapdrmprotend!', start)
    return data[start:stop]


if __name__ == '__main__':
    with open(sys.argv[1], 'rb') as f:
        inner = unwrap(f.read())
    with open(sys.argv[2], 'wb') as f:
        f.write(inner)
