#!/usr/bin/env python3
"""Compare two rendered surfaces strictly enough to catch a real defect.

The cross-backend replay compared a per-channel *mean* over the whole frame,
for a stated and correct reason: a CPU rasteriser and a GPU disagree
legitimately about interpolation precision and about which pixel a primitive
edge lands on, so an exact digest is unusable. But a mean is blind to almost
everything that matters. **A vertically flipped image has exactly the same
mean and exactly the same coverage** -- and rendering a scene upside down
is a real bug to catch.

The way out is that legitimate disagreement is *confined to primitive edges*:
a one-pixel fringe along a boundary. A real defect -- a dropped draw, a wrong
texture, a mirrored image, a bad blend -- differs across a region's interior.

So: build the per-pixel difference mask, then **erode** it. A differing pixel
survives only if everything within `--erode` of it also differs, which no
edge fringe does. What is left is a defect, and the count of surviving pixels
is a number that can be driven to zero.

Surfaces are the `.gexsurf` container tests/gl-replay.mjs writes: the eight
bytes `GEXSURF\\0`, width and height as little-endian uint32, then rows of
RGBA8 bottom-up, as glReadPixels returns them.
"""
import argparse
import struct
import sys

MAGIC = b"GEXSURF\0"


def read_surface(path):
    with open(path, "rb") as handle:
        blob = handle.read()
    if len(blob) < 16 or blob[:8] != MAGIC:
        raise SystemExit(f"{path}: not a .gexsurf container")
    width, height = struct.unpack_from("<II", blob, 8)
    pixels = blob[16:]
    if len(pixels) < width * height * 4:
        raise SystemExit(f"{path}: truncated: {len(pixels)} bytes for "
                         f"{width}x{height}")
    return width, height, pixels[:width * height * 4]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("reference")
    parser.add_argument("candidate")
    # 8/255 absorbs interpolation precision without hiding anything a person
    # would see.
    parser.add_argument("--tolerance", type=int, default=8)
    # One pixel removes the edge fringe; two is for a backend that also
    # disagrees about coverage on a diagonal.
    parser.add_argument("--erode", type=int, default=1)
    parser.add_argument("--max-pixels", type=int, default=0)
    parser.add_argument("--label", default="surface")
    arguments = parser.parse_args()

    width, height, reference = read_surface(arguments.reference)
    cwidth, cheight, candidate = read_surface(arguments.candidate)
    if (width, height) != (cwidth, cheight):
        print(f"SURFACE {arguments.label} FAIL size {cwidth}x{cheight} "
              f"!= reference {width}x{height}")
        return 1

    from PIL import Image, ImageFilter

    reference_image = Image.frombytes("RGBA", (width, height), reference)
    candidate_image = Image.frombytes("RGBA", (width, height), candidate)

    # Per-pixel, per-channel absolute difference, reduced to the worst channel.
    worst = None
    for reference_band, candidate_band in zip(reference_image.split(),
                                              candidate_image.split()):
        from PIL import ImageChops
        band = ImageChops.difference(reference_band, candidate_band)
        worst = band if worst is None else ImageChops.lighter(worst, band)

    raw = worst.point(lambda v: 255 if v > arguments.tolerance else 0)
    differing = sum(1 for v in raw.tobytes() if v)

    # MinFilter over a binary image is erosion: a pixel keeps its value only
    # if every pixel in the window has it.
    eroded = raw
    if arguments.erode > 0:
        eroded = raw.filter(ImageFilter.MinFilter(2 * arguments.erode + 1))
    surviving_data = list(eroded.tobytes())
    surviving = sum(1 for v in surviving_data if v)

    total = width * height
    print(f"SURFACE {arguments.label} {width}x{height} "
          f"differing={differing} ({100.0 * differing / total:.2f}%) "
          f"interior={surviving} ({100.0 * surviving / total:.2f}%) "
          f"tolerance={arguments.tolerance} erode={arguments.erode}")

    if surviving > arguments.max_pixels:
        # Where, so the next question has a starting point. Rows are reported
        # in glReadPixels order, which is bottom-up.
        rows = [y for y in range(height)
                if any(surviving_data[y * width + x] for x in range(width))]
        columns = [x for x in range(width)
                   if any(surviving_data[y * width + x] for y in range(height))]
        print(f"SURFACE {arguments.label} FAIL {surviving} interior pixels "
              f"differ (allowed {arguments.max_pixels}); "
              f"rows {rows[0]}..{rows[-1]} columns {columns[0]}..{columns[-1]}",
              file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
