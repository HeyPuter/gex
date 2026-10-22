#!/usr/bin/env python3
"""Summarise a rendered frame, so a gate can check the picture and not just
the frame count.

A game can present every frame while its scene renders almost entirely
black -- the counter cannot see that, and a closing line that says
"correctness needs the screenshot" has nobody reading it. These
are the cheapest statistics that separate a rendered scene from a black one, a
flat one, or a single-colour one:

  mean      average luminance, 0..1
  stdev     luminance spread; a scene has texture and shading, a fill does not
  dark      fraction of pixels below 8/255, which is what an unbound sampler
            or a failed compressed-texture decode leaves behind
  distinct  how many of a 4x4x4 colour-cube's cells the image touches

Prints one `FRAME` line, and exits non-zero if a --min-* / --max-* bound
given on the command line is not met.
"""
import argparse
import sys

from PIL import Image


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("image")
    parser.add_argument("--min-mean", type=float)
    parser.add_argument("--max-mean", type=float)
    parser.add_argument("--min-stdev", type=float)
    parser.add_argument("--max-dark", type=float)
    parser.add_argument("--min-distinct", type=int)
    # A screenshot of the whole page includes the surrounding document, which
    # is neither the application's output nor interesting; --crop takes the
    # centre fraction so the statistics describe the canvas.
    parser.add_argument("--crop", type=float, default=1.0)
    arguments = parser.parse_args()

    image = Image.open(arguments.image).convert("RGB")
    if arguments.crop < 1.0:
        width, height = image.size
        keep_w, keep_h = width * arguments.crop, height * arguments.crop
        left, top = (width - keep_w) / 2, (height - keep_h) / 2
        image = image.crop((int(left), int(top), int(left + keep_w),
                            int(top + keep_h)))
    raw = image.tobytes()
    pixels = [(raw[i], raw[i + 1], raw[i + 2]) for i in range(0, len(raw), 3)]
    count = len(pixels)
    if not count:
        print("FRAME empty")
        return 1

    luminance = [(0.2126 * r + 0.7152 * g + 0.0722 * b) / 255.0
                 for r, g, b in pixels]
    mean = sum(luminance) / count
    variance = sum((value - mean) ** 2 for value in luminance) / count
    stdev = variance ** 0.5
    dark = sum(1 for value in luminance if value < 8 / 255.0) / count
    cells = {(r >> 6, g >> 6, b >> 6) for r, g, b in pixels}

    print(f"FRAME mean={mean:.3f} stdev={stdev:.3f} dark={dark:.3f} "
          f"distinct={len(cells)} pixels={count}")

    failures = []
    if arguments.min_mean is not None and mean < arguments.min_mean:
        failures.append(f"mean {mean:.3f} < {arguments.min_mean}")
    if arguments.max_mean is not None and mean > arguments.max_mean:
        failures.append(f"mean {mean:.3f} > {arguments.max_mean}")
    if arguments.min_stdev is not None and stdev < arguments.min_stdev:
        failures.append(f"stdev {stdev:.3f} < {arguments.min_stdev}")
    if arguments.max_dark is not None and dark > arguments.max_dark:
        failures.append(f"dark {dark:.3f} > {arguments.max_dark}")
    if (arguments.min_distinct is not None
            and len(cells) < arguments.min_distinct):
        failures.append(f"distinct {len(cells)} < {arguments.min_distinct}")
    for failure in failures:
        print(f"FRAME FAIL {failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
