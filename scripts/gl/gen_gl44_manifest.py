#!/usr/bin/env python3
"""Generate transport metadata for the OpenGL 4.4 core command set.

glcorearb.h defines the exact core-profile surface while Khronos' gl.xml adds
machine-readable parameter length, group, and semantic-kind metadata.  The
result is deliberately data rather than generated C: transport generators can
then share one reviewed command model for the C guest, C++ executor, tests,
and documentation.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import xml.etree.ElementTree as ET

from gl44_coverage import VERSIONS, version_commands


def element_text(element: ET.Element) -> str:
    return "".join(element.itertext()).strip()


def normalize_space(value: str) -> str:
    return re.sub(r"\s+", " ", value).strip()


def command_record(command: ET.Element, version: str) -> dict:
    proto = command.find("proto")
    if proto is None:
        raise RuntimeError("command without prototype")
    name_node = proto.find("name")
    type_node = proto.find("ptype")
    if name_node is None:
        raise RuntimeError("command prototype without a name")
    name = name_node.text or ""
    return_type = element_text(type_node) if type_node is not None else normalize_space(
        element_text(proto).replace(name, "", 1)
    )
    params = []
    for param in command.findall("param"):
        param_name_node = param.find("name")
        param_type_node = param.find("ptype")
        if param_name_node is None:
            raise RuntimeError(f"{name}: parameter without a name")
        param_name = param_name_node.text or ""
        declaration = normalize_space(element_text(param))
        param_type = element_text(param_type_node) if param_type_node is not None else "void"
        params.append(
            {
                "name": param_name,
                "type": param_type,
                "declaration": declaration,
                "pointer": "*" in declaration,
                "const": bool(re.search(r"\bconst\b", declaration)),
                "length": param.get("len"),
                "group": param.get("group"),
                "kind": param.get("kind"),
            }
        )
    return {
        "name": name,
        "introduced": version,
        "return_type": return_type,
        "params": params,
        "alias": command.get("alias"),
        "vecequiv": command.get("vecequiv"),
    }


def main() -> int:
    root = pathlib.Path(__file__).resolve().parents[2]
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=pathlib.Path)
    args = parser.parse_args()

    registry_root = root / "third_party/opengl-registry"
    registry_path = registry_root / "xml/gl.xml"
    by_version = version_commands((registry_root / "api/GL/glcorearb.h").read_text())
    introduced = {}
    for major, minor in VERSIONS:
        version = f"{major}.{minor}"
        for name in by_version[version]:
            introduced.setdefault(name, version)

    registry = ET.parse(registry_path).getroot()
    xml_commands = {}
    commands_node = registry.find("commands")
    if commands_node is None:
        raise RuntimeError("gl.xml has no commands section")
    for command in commands_node.findall("command"):
        proto_name = command.findtext("proto/name")
        if proto_name:
            xml_commands[proto_name] = command

    absent = sorted(set(introduced) - set(xml_commands))
    if absent:
        raise RuntimeError("gl.xml lacks core commands: " + ", ".join(absent))
    commands = [
        command_record(xml_commands[name], introduced[name]) for name in sorted(introduced)
    ]
    manifest = {
        "api": "OpenGL",
        "profile": "core",
        "version": "4.4",
        "registry_revision": subprocess_revision(root / "third_party/opengl-registry"),
        "commands": commands,
    }
    encoded = json.dumps(manifest, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(encoded)
    else:
        print(encoded, end="")
    return 0


def subprocess_revision(repository: pathlib.Path) -> str:
    head = repository / ".git"
    # A submodule's .git is normally a `gitdir:` indirection. Avoid invoking
    # Git in the generator so manifest creation stays hermetic in source
    # archives; the checked-out HEAD file is enough in either layout.
    git_dir = repository / ".git"
    if git_dir.is_file():
        target = git_dir.read_text().strip().removeprefix("gitdir:").strip()
        git_dir = (repository / target).resolve()
    head_text = (git_dir / "HEAD").read_text().strip()
    if head_text.startswith("ref:"):
        ref = head_text.removeprefix("ref:").strip()
        loose = git_dir / ref
        if loose.is_file():
            return loose.read_text().strip()
        packed = git_dir / "packed-refs"
        if packed.is_file():
            for line in packed.read_text().splitlines():
                if line and not line.startswith(("#", "^")):
                    revision, name = line.split(" ", 1)
                    if name == ref:
                        return revision
        raise RuntimeError(f"cannot resolve registry revision {ref}")
    return head_text


if __name__ == "__main__":
    raise SystemExit(main())
