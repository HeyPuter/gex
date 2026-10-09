#!/usr/bin/env python3
"""Generate the guest's GL extension allowlist and command aliases.

The guest used to answer glGetStringi(GL_EXTENSIONS) from a hardcoded list of
three names and never asked the host at all. That predates the Mesa path,
where the host reports well over two hundred, and it made every driver
capability invisible: an application cannot use textureGather it has no way to
detect, so exposing a cap without this is exposing nothing.

Forwarding the host's list wholesale is not safe either. An extension is a
promise about entry points as well as about behaviour, and the guest exports a
fixed set of functions; advertising an extension whose commands are missing
turns a graceful fallback into a null call. So the list is filtered: an
extension is claimable only when every command the registry says it requires
is exported by the guest library itself.

That makes the library its own source of truth -- the allowlist is derived
from `nm` on the linked object, not from a hand-kept table -- and it fails
closed, which is the same rule tools/gen_gl44_transport.py follows: an
extension the registry does not describe is not claimed.

Most of what that first withheld was not missing behaviour but a missing
spelling. GL_ARB_vertex_buffer_object requires glBufferDataARB, which is the
same function as glBufferData -- the registry says so with <alias> -- and the
guest exported only the unsuffixed name. Ten extensions applications routinely
test for, instancing and vertex buffer objects among them, were withheld over
nothing but a suffix. So this also emits those aliases, for exported commands
only, and counts them as exports when deciding what is claimable.
"""
import argparse
import pathlib
import subprocess
import sys
import xml.etree.ElementTree as ElementTree

# The desktop APIs the host can present, plus the ES ones, because the guest
# serves both and the filter is about entry points rather than about profile.
APIS = {"gl", "glcore", "gles1", "gles2"}


def extension_commands(registry: pathlib.Path) -> dict[str, set[str]]:
    """Every registry extension, mapped to the commands it requires."""
    root = ElementTree.parse(registry).getroot()
    commands: dict[str, set[str]] = {}
    for extension in root.iterfind("./extensions/extension"):
        name = extension.get("name")
        if not name:
            continue
        supported = set((extension.get("supported") or "").split("|"))
        if not supported & APIS:
            continue
        required: set[str] = set()
        for require in extension.iterfind("require"):
            # A <require> tagged for one API describes that API only; the
            # untagged ones apply to every API the extension supports.
            api = require.get("api")
            if api is not None and api not in APIS:
                continue
            # The guest presents a core profile -- glGetIntegerv reports
            # GL_CONTEXT_CORE_PROFILE_BIT -- so the commands a <require>
            # reserves for compatibility do not exist in it and cannot be
            # what makes an extension unclaimable. This is the line to
            # revisit if a compatibility context is ever offered:
            # GL_ARB_vertex_type_2_10_10_10_rev asks for 38 commands and 30
            # of them are the compatibility glColorP3ui family.
            if require.get("profile") == "compatibility":
                continue
            for command in require.iterfind("command"):
                if command.get("name"):
                    required.add(command.get("name"))
        commands[name] = required
    return commands


def exported_functions(library: pathlib.Path) -> set[str]:
    """The GL entry points the guest library actually defines."""
    output = subprocess.run(
        ["nm", "--dynamic", "--defined-only", str(library)],
        check=True, capture_output=True, text=True).stdout
    names = set()
    for line in output.splitlines():
        fields = line.split()
        # Global text symbols only: a GL name that resolved to data, or to a
        # local, is not an entry point an application can call.
        if len(fields) == 3 and fields[1] == "T" and fields[2].startswith("gl"):
            names.add(fields[2])
    return names


# Suffixed spellings the registry does not mark as aliases, but which this
# implementation can answer with the core command.
#
# EXT_framebuffer_object predates ARB_framebuffer_object and Mesa implements
# the two with separate entry points, because EXT lets an application bind a
# framebuffer or renderbuffer name it never generated and the core function
# does not. That is the whole of the difference, and it is the only thing
# these two aliases get wrong.
#
# The alternative was worse. The host advertises GL_EXT_framebuffer_object,
# this library withheld it for want of two symbols, and glmark2 -- which
# checks for that extension by name and not for the ARB one -- concluded the
# backend had no framebuffer support at all and skipped four of its scenes.
# Anything added here needs that shape of justification: the host really has
# the extension, and the difference is one an application is unlikely to be
# relying on.
CURATED_ALIASES = {
    "glBindFramebufferEXT": "glBindFramebuffer",
    "glBindRenderbufferEXT": "glBindRenderbuffer",
}


# Extensions that add no commands of their own but are options on another
# extension's programs, which the registry does not record: with nothing
# required they were always "claimable", while the extension they extend was
# withheld for its missing commands (glGenProgramsARB and the rest). An
# application that checks only the option then calls a null pointer. wined3d
# does exactly that: its caps probe tests a vertex program with OPTION
# NV_vertex_program2 when GL_NV_vertex_program2_option is listed, and on
# NVIDIA's host (which lists it) Undertale and Katana ZERO died in
# glGenProgramsARB at address 0. Each is claimable only with its base.
BUILDS_ON = {
    "GL_ARB_fragment_program_shadow": "GL_ARB_fragment_program",
    "GL_NV_fragment_program_option": "GL_ARB_fragment_program",
    "GL_NV_fragment_program2": "GL_ARB_fragment_program",
    "GL_NV_vertex_program2_option": "GL_ARB_vertex_program",
    "GL_NV_vertex_program3": "GL_ARB_vertex_program",
    "GL_NV_vertex_program1_1": "GL_NV_vertex_program",
    "GL_NV_vertex_program2": "GL_NV_vertex_program",
}


def command_aliases(registry: pathlib.Path) -> dict[str, str]:
    """Commands the registry declares to be another command under a new name."""
    root = ElementTree.parse(registry).getroot()
    aliases = {}
    for command in root.iterfind("./commands/command"):
        alias = command.find("alias")
        name = command.find("./proto/name")
        if alias is not None and name is not None and name.text:
            aliases[name.text] = alias.get("name")
    aliases.update(CURATED_ALIASES)
    return aliases


def generate(registry: pathlib.Path, library: pathlib.Path) -> tuple[str, str]:
    commands = extension_commands(registry)
    exported = exported_functions(library)
    aliases = command_aliases(registry)

    # What the library defines itself, as opposed to what a previous run of
    # this generator added. Every registry alias is a second spelling of some
    # other command, so nothing here should define one -- and subtracting them
    # is what makes this idempotent. Without it the set of aliases to emit is
    # computed against a library that already has them, comes out empty, and
    # the relink drops all of them again: the musl and glibc builds of the
    # same source ended up with 974 and 525 exports.
    defined = exported - aliases.keys()

    # Only aliases some supported extension actually asks for, so the library
    # does not grow a symbol nothing would ever look up.
    wanted = set().union(*commands.values()) if commands else set()
    emit = sorted(name for name in wanted - defined
                  if aliases.get(name) in defined)
    alias_lines = [
        "/* Generated by tools/gen_gl_extensions.py; do not edit.",
        " *",
        " * The registry's <alias>: one command that is another command under",
        " * a different name. An extension requires the suffixed spelling even",
        " * when the function has been core, unsuffixed, for twenty years, and",
        " * an extension whose entry points are missing cannot honestly be",
        f" * claimed -- so here are the {len(emit)} of them whose target this",
        " * library already exports. Included last, where every target is",
        " * declared, and typed from the target so the two cannot disagree.",
        " */",
    ]
    alias_lines += [
        f"extern __typeof({aliases[name]}) {name} "
        f'__attribute__((alias("{aliases[name]}")));'
        for name in emit
    ]
    alias_lines.append("")

    available = defined | set(emit)
    claimable = {name for name, required in commands.items()
                 if required <= available}
    claimable -= {name for name, base in BUILDS_ON.items() if base not in claimable}
    claimable = sorted(claimable)
    withheld = len(commands) - len(claimable)
    lines = [
        "/* Generated by tools/gen_gl_extensions.py; do not edit.",
        " *",
        " * Extensions the guest may repeat from the host: every command the",
        " * Khronos registry says they require is exported by this library.",
        f" * {len(claimable)} claimable, {withheld} withheld for missing entry",
        " * points, out of the registry's whole extension set. A host",
        " * extension absent from this table is not advertised to the guest,",
        " * which is why adding a driver capability can need a rebuild here.",
        " */",
        "static const char *const gexgl_claimable_extensions[] = {",
    ]
    lines += [f'    "{name}",' for name in claimable]
    lines += ["};", ""]
    return "\n".join(lines), "\n".join(alias_lines)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--library", required=True, type=pathlib.Path)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("--aliases", required=True, type=pathlib.Path)
    parser.add_argument("--registry", type=pathlib.Path)
    arguments = parser.parse_args()
    root = pathlib.Path(__file__).resolve().parents[1]
    registry = arguments.registry or (
        root / "third_party/opengl-registry/xml/gl.xml")
    if not registry.is_file():
        print(f"no registry at {registry}", file=sys.stderr)
        return 1
    outputs = dict(zip((arguments.output, arguments.aliases),
                       generate(registry, arguments.library)))
    changed = False
    for path, value in outputs.items():
        if path.is_file() and path.read_text() == value:
            continue
        path.write_text(value)
        changed = True
    # The caller relinks on this: both files are compiled in, so a change to
    # either only reaches the library through another build. The alias file
    # adds exported symbols, which is why the allowlist is computed against
    # them here rather than waiting for the next `nm`.
    if changed:
        print("changed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
