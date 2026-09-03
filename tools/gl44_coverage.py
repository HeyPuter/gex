#!/usr/bin/env python3
"""Measure command routing in the independently testable layers of gex's GL path.

The command list comes from Khronos' generated glcorearb.h in the pinned
MobileGL checkout.  An entry point is not counted merely because a header
declares it: the guest shared object must export it, MobileGL must return it
from GetProcAddress, and backend operations must have a non-null DirectWebGPU
function-table slot.

This is deliberately *not* a conformance metric.  Exporting a command, returning
it from GetProcAddress, or wiring a backend slot says nothing about validation,
state semantics, output pixels, synchronization, or memory safety.  In
particular, the backend table contains only the operations MobileGL chose to
delegate and may wire intentional no-ops, so its percentage is a wiring audit
over that table rather than coverage of OpenGL.
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
    # Through 4.5, not 4.4: the driver now grants GL 4.5 extensions and the
    # guest has to be able to export their entry points. glTextureBarrier is
    # 4.5 core, so with the universe stopping at 4.4 the transport generator
    # refused it ("commands absent from GL 4.4 core") and the guest could
    # claim neither ARB_texture_barrier nor NV_texture_barrier however the
    # host driver was built.
    #
    # This widens the *universe* only: the transport generator still emits
    # exactly the commands its policy sets name, so nothing new is generated
    # by adding a version here.
    + [(4, minor) for minor in range(6)]
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
    root = pathlib.Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--guest",
        type=pathlib.Path,
        default=root / "web/public/fs/libGLESv2.so.2",
        help="built guest libGLES shared object inspected with nm -D",
    )
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--min-guest", type=int, default=0)
    parser.add_argument("--min-backend", type=int, default=0)
    parser.add_argument(
        "--mobilegl",
        type=pathlib.Path,
        default=root / "third_party/mobilegl-webgpu",
        help="effective MobileGL source tree to inspect",
    )
    parser.add_argument(
        "--backend-patch",
        type=pathlib.Path,
        action="append",
        default=[],
        help="ordered patch contributing DirectWebGPU function-table wiring",
    )
    parser.add_argument(
        "--require",
        choices=("guest", "loader", "frontend", "backend", "all"),
        action="append",
        default=[],
        help="fail unless the selected layer covers every required command",
    )
    args = parser.parse_args()

    mobilegl = args.mobilegl.resolve()
    header = (mobilegl / "include/GL/glcorearb.h").read_text()
    by_version = version_commands(header)
    required = set().union(*by_version.values())

    if not args.guest.is_file():
        parser.error(f"guest library does not exist: {args.guest}")
    guest = dynamic_exports(args.guest)

    getproc_source = (mobilegl / "MobileGL/MG_Impl/GetProcAddress.cpp").read_text()
    loader = set(re.findall(r"GETPROC\((gl[A-Z]\w+)", getproc_source))
    definitions_source = (
        mobilegl / "MobileGL/MG_Impl/GLImpl/Exporting/Definitions.cpp"
    ).read_text()
    frontend_stubs = {
        "gl" + name
        for name in re.findall(
            r"DECLARE_GL_FUNCTION_STUB_HEAD\([^,\n]+,\s*(\w+)", definitions_source
        )
    }
    frontend = loader - frontend_stubs

    table_source = (mobilegl / "MobileGL/MG_Backend/BackendObject.h").read_text()
    backend_fields = set(re.findall(r"\(\*(\w+)\)\s*\(", table_source))
    direct_source = (
        mobilegl / "MobileGL/MG_Backend/DirectWebGPU/BackendObject_DirectWebGPU.cpp"
    ).read_text()
    backend_wired = set(re.findall(r"\.GL\.(\w+)\s*=", direct_source))
    direct_path = "b/MobileGL/MG_Backend/DirectWebGPU/BackendObject_DirectWebGPU.cpp"
    for patch_path in args.backend_patch:
        if not patch_path.is_file():
            parser.error(f"backend patch does not exist: {patch_path}")
        in_direct_file = False
        for line in patch_path.read_text().splitlines():
            if line.startswith("diff --git "):
                in_direct_file = line.split()[-1] == direct_path
                continue
            if not in_direct_file or line.startswith(("+++", "---")):
                continue
            match = re.search(r"\.GL\.(\w+)\s*=", line[1:])
            if not match:
                continue
            if line.startswith("+"):
                backend_wired.add(match.group(1))
            elif line.startswith("-"):
                backend_wired.discard(match.group(1))
    backend_required = {name[2:] for name in required if name[2:] in backend_fields}

    layers = {
        "guest": {
            "covered": len(required & guest),
            "required": len(required),
            "percent": percent(len(required & guest), len(required)),
            "missing": sorted(required - guest),
        },
        "frontend": {
            "covered": len(required & frontend),
            "required": len(required),
            "percent": percent(len(required & frontend), len(required)),
            "missing": sorted(required - frontend),
        },
        "loader": {
            "covered": len(required & loader),
            "required": len(required),
            "percent": percent(len(required & loader), len(required)),
            "missing": sorted(required - loader),
        },
        "backend": {
            "covered": len(backend_required & backend_wired),
            "required": len(backend_required),
            "percent": percent(len(backend_required & backend_wired), len(backend_required)),
            "missing": sorted("gl" + name for name in backend_required - backend_wired),
            "meaning": "DirectWebGPU function-table wiring only; no-op and semantically incomplete slots count",
        },
    }
    cumulative: set[str] = set()
    versions = {}
    for version, commands in by_version.items():
        cumulative |= commands
        versions[version] = {
            "required": len(cumulative),
            "guest": len(cumulative & guest),
                "frontend": len(cumulative & frontend),
                "loader": len(cumulative & loader),
        }
    report = {
        "target": "OpenGL 4.4 core command routing (not conformance)",
        "registry": "third_party/mobilegl-webgpu/include/GL/glcorearb.h",
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
        print("cumulative transport coverage by core version:")
        for version, row in versions.items():
            print(
                f"  {version:3} guest={row['guest']:3}/{row['required']:3} "
                f"frontend={row['frontend']:3}/{row['required']:3} "
                f"loader={row['loader']:3}/{row['required']:3}"
            )

    required_layers = set(args.require)
    if "all" in required_layers:
        required_layers = set(layers)
    failed = [name for name in required_layers if layers[name]["missing"]]
    if layers["guest"]["covered"] < args.min_guest:
        failed.append(f"guest<{args.min_guest}")
    if layers["backend"]["covered"] < args.min_backend:
        failed.append(f"backend<{args.min_backend}")
    if failed:
        print("incomplete required layers: " + ", ".join(sorted(failed)), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
