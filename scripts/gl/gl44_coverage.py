#!/usr/bin/env python3
"""Measure OpenGL 4.4 core command routing in the guest's GL library.

The command list comes from Khronos' generated glcorearb.h in the pinned
OpenGL-Registry checkout.  A command counts when the guest shared object
exports it, which is what an application's dlsym or GetProcAddress finds.

This is deliberately *not* a conformance metric.  Exporting a command says
nothing about validation, state semantics, output pixels, synchronization, or
memory safety.  On the host, every command the wire carries runs through
Mesa's st/mesa frontend.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import subprocess
import sys


VERSIONS = (
    [(1, minor) for minor in range(6)]
    + [(2, minor) for minor in range(2)]
    + [(3, minor) for minor in range(4)]
    + [(4, minor) for minor in range(5)]
)


def version_commands(header: str) -> dict[str, set[str]]:
    result: dict[str, set[str]] = {}
    for major, minor in VERSIONS:
        version = f"{major}.{minor}"
        match = re.search(
            rf"#ifndef GL_VERSION_{major}_{minor}\b(.*?)"
            rf"#endif /\* GL_VERSION_{major}_{minor} \*/",
            header,
            re.S,
        )
        if not match:
            raise RuntimeError(f"Khronos header has no GL_VERSION_{major}_{minor} block")
        result[version] = set(re.findall(r"\b(gl[A-Z]\w+)\s*\(", match.group(1)))
    return result


def dynamic_exports(shared_object: pathlib.Path) -> set[str]:
    output = subprocess.check_output(["nm", "-D", str(shared_object)], text=True)
    return set(re.findall(r"\bT (gl[A-Z]\w+)$", output, re.M))


def percent(have: int, total: int) -> float:
    return round(100.0 * have / total, 1) if total else 100.0


def main() -> int:
    root = pathlib.Path(__file__).resolve().parents[2]
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--guest",
        type=pathlib.Path,
        default=root / "page/public/fs/libGLESv2.so.2",
        help="built guest libGLES shared object inspected with nm -D",
    )
    parser.add_argument(
        "--registry",
        type=pathlib.Path,
        default=root / "third_party/opengl-registry",
        help="OpenGL-Registry checkout providing api/GL/glcorearb.h",
    )
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--min-guest", type=int, default=0)
    parser.add_argument(
        "--require",
        choices=("guest",),
        action="append",
        default=[],
        help="fail unless the guest exports every required command",
    )
    args = parser.parse_args()

    header = (args.registry.resolve() / "api/GL/glcorearb.h").read_text()
    by_version = version_commands(header)
    required = set().union(*by_version.values())

    if not args.guest.is_file():
        parser.error(f"guest library does not exist: {args.guest}")
    guest = dynamic_exports(args.guest)

    layers = {
        "guest": {
            "covered": len(required & guest),
            "required": len(required),
            "percent": percent(len(required & guest), len(required)),
            "missing": sorted(required - guest),
        },
    }
    cumulative: set[str] = set()
    versions = {}
    for version, commands in by_version.items():
        cumulative |= commands
        versions[version] = {
            "required": len(cumulative),
            "guest": len(cumulative & guest),
        }
    report = {
        "target": "OpenGL 4.4 core command routing (not conformance)",
        "registry": "third_party/opengl-registry/api/GL/glcorearb.h",
        "commands": len(required),
        "layers": layers,
        "versions": versions,
    }

    if args.json:
        print(json.dumps(report, indent=2, sort_keys=True))
    else:
        print(f"{report['target']}: {len(required)} commands")
        for name, layer in layers.items():
            print(
                f"{name:8} {layer['covered']:3}/{layer['required']:3} "
                f"({layer['percent']:5.1f}%)  missing={len(layer['missing'])}"
            )
        print("cumulative guest coverage by core version:")
        for version, row in versions.items():
            print(f"  {version:3} guest={row['guest']:3}/{row['required']:3}")

    failed = [name for name in set(args.require) if layers[name]["missing"]]
    if layers["guest"]["covered"] < args.min_guest:
        failed.append(f"guest<{args.min_guest}")
    if failed:
        print("incomplete required layers: " + ", ".join(sorted(failed)), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
