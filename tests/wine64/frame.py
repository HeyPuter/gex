#!/usr/bin/env python3
"""Checks the frames gexc's compositor wrote (--wayland-out PREFIX:
PREFIX-NNN.ppm, binary P6) for a picture: passes when one frame has at
least MIN pixels of every COLOR (R,G,B, exact; R,G,B:N wants N of it).
Environment: FRAME_SKIP=WxH leaves out the frames of that size (another
surface's, such as a GL child's); FRAME_LAST=N wants each of the last N
frames to have them instead (a picture that has to stay).

    frame.py PREFIX MIN COLOR...

Prints the frame that matched, or each colour's best count."""
import glob, os, sys

prefix, need = sys.argv[1], int(sys.argv[2])
colors, needs = [], {}
for a in sys.argv[3:]:
    c, _, n = a.partition(':')
    colors.append(tuple(map(int, c.split(','))))
    needs[colors[-1]] = int(n) if n else need

def pixels(path):
    data = open(path, 'rb').read()
    fields, i = [], 0
    while len(fields) < 4:  # magic, width, height, maxval; '#' comments
        while data[i:i + 1].isspace(): i += 1
        if data[i:i + 1] == b'#':
            i = data.index(b'\n', i)
            continue
        j = i
        while not data[j:j + 1].isspace(): j += 1
        fields.append(data[i:j]); i = j
    assert fields[0] == b'P6' and fields[3] == b'255', path
    return data[i + 1:]

def size(path):
    with open(path, 'rb') as f:
        head = f.read(64).split()
    return f'{int(head[1])}x{int(head[2])}'

best = {c: 0 for c in colors}
frames = sorted(glob.glob(glob.escape(prefix) + '-*.ppm'))
skip = os.environ.get('FRAME_SKIP')
if skip:
    frames = [f for f in frames if size(f) != skip]
last = int(os.environ.get('FRAME_LAST') or 0)
if last:
    tail = frames[-last:]
    for f in tail:
        px = pixels(f)
        count = {bytes(c): px.count(bytes(c)) for c in colors}
        if not all(count[bytes(c)] >= needs[c] for c in colors):
            print(f'frame {f} (of the last {len(tail)}) lacks a colour: ' + ' '.join(f'{c}={count[bytes(c)]}>={needs[c]}' for c in colors))
            sys.exit(1)
    print(f'the last {len(tail)} of {len(frames)} frames have each colour')
    sys.exit(0 if len(tail) == last else 1)
for f in frames:
    px = pixels(f)
    # bytes.count: a colour's three bytes met across two pixels count too,
    # which these distinct colours make rare enough not to matter
    count = {bytes(c): px.count(bytes(c)) for c in colors}
    for c in colors: best[c] = max(best[c], count[bytes(c)])
    if all(count[bytes(c)] >= needs[c] for c in colors):
        print(f'frame {f}: ' + ' '.join(f'{c}={count[bytes(c)]}' for c in colors))
        sys.exit(0)
print(f'no frame of {len(frames)} has enough of each colour ({" ".join(f"{c}>={needs[c]}" for c in colors)}); best: ' + ' '.join(f'{c}={n}' for c, n in best.items()))
sys.exit(1)
