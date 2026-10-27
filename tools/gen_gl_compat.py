#!/usr/bin/env python3
"""Generate gex-gl's compatibility-profile transport from the Khronos registry.

    tools/gen_gl_compat.py [--check]

Every OpenGL command in third_party/opengl-registry/xml/gl.xml -- GL 1.0
through 4.6 including what the core profile removed, and every extension
command -- gets a wire *plan* derived from the registry's own parameter
metadata (types, `len`, `class`), unless something about it cannot be
carried safely (a callback, a pointer to pointers, a length the registry
leaves to prose); those are listed in the generated files with the reason.

The plan is a short string, one token per parameter, that both encoders
interpret and the host decoders are generated from:

    v | w | f | q          the return: none, a 32-bit word, a float, 64 bits
    w                      a 32-bit scalar parameter: one word
    q                      a 64-bit scalar (doubles, 64-bit integers, and
                           GLintptr/GLsizeiptr, sign-extended): two words
    i<size>:<count>        an input array of <size>-byte elements
    j<size>:<count>        the same, pixel data: an offset into the bound
                           unpack buffer when there is one
    o<size>:<count>        an output array (comes back in the reply)

and a count is

    n<N>         N elements                  p<i>[*<M>]   parameter i, times M
    e<i>         pname-sized (gl-pname-counts.h) on parameter i
    s            a NUL-terminated string     S<i>         a string of length
                                                          parameter i (<0: NUL)
    z<i>         parameter i bytes           L<i>,<j>     parameter i elements
                                                          of GL type parameter j
    X<f>,<t>,<w>,<h>,<d>  an unpack pixel transfer (format, type, width,
                 height, depth; `_` for absent): from the unpack buffer when
                 one is bound
    B<w>,<h>     a glBitmap bitmap          T            glPolygonStipple's
                                                          32x32 bits

On the wire a command is its scalars in order, each input array as a tag
word (0 null, 1 a blob follows: length then bytes padded to four, 2 an
offset into the bound unpack buffer: two words) and nothing for outputs;
the reply is the return value then each output as a blob. Opcodes are
COMPAT_BASE plus the command's index in the registry's sorted command list,
so a policy change never renumbers one.

Outputs:
  guest/gl/gl-compat-generated.inc      the Linux guest's entry points
  guest/gl/gl-pname-counts.h            the pname-sized counts (C, both ends)
  crates/gex-gl/src/compat_plans.rs     the opcode table, for the hosts' tests
  web/mesa/gl-compat-generated-cases.inc        the host decoder (C++; the
                                        browser's Mesa and the native host)
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys
import xml.etree.ElementTree as ET

ROOT = pathlib.Path(__file__).resolve().parents[1]
REGISTRY = ROOT / "third_party/opengl-registry/xml/gl.xml"
COMPAT_BASE = 8192

# Scalar types: (wire token, guest C type, host C type, element bytes).
SCALARS = {
    "GLenum": ("w", "unsigned", "GLenum", 4),
    "GLuint": ("w", "unsigned", "GLuint", 4),
    "GLbitfield": ("w", "unsigned", "GLbitfield", 4),
    "GLint": ("w", "int", "GLint", 4),
    "GLsizei": ("w", "int", "GLsizei", 4),
    "GLfixed": ("w", "int", "GLint", 4),
    "GLclampx": ("w", "int", "GLint", 4),
    "GLhandleARB": ("w", "unsigned", "GLuint", 4),
    "GLshort": ("w", "short", "GLshort", 2),
    "GLushort": ("w", "unsigned short", "GLushort", 2),
    "GLhalfNV": ("w", "unsigned short", "GLushort", 2),
    "GLbyte": ("w", "signed char", "GLbyte", 1),
    "GLubyte": ("w", "unsigned char", "GLubyte", 1),
    "GLboolean": ("w", "unsigned char", "GLboolean", 1),
    "GLchar": ("w", "char", "GLchar", 1),
    "GLcharARB": ("w", "char", "GLchar", 1),
    "GLfloat": ("f", "float", "GLfloat", 4),
    "GLclampf": ("f", "float", "GLfloat", 4),
    "GLdouble": ("q", "double", "GLdouble", 8),
    "GLclampd": ("q", "double", "GLdouble", 8),
    "GLint64": ("q", "long long", "GLint64", 8),
    "GLint64EXT": ("q", "long long", "GLint64", 8),
    "GLuint64": ("q", "unsigned long long", "GLuint64", 8),
    "GLuint64EXT": ("q", "unsigned long long", "GLuint64", 8),
    "GLintptr": ("q", "long", "ptrdiff_t", None),
    "GLsizeiptr": ("q", "long", "ptrdiff_t", None),
    "GLintptrARB": ("q", "long", "ptrdiff_t", None),
    "GLsizeiptrARB": ("q", "long", "ptrdiff_t", None),
}
# Array element types (pointer-sized integers are not carried in arrays).
ELEMENTS = {k: v for k, v in SCALARS.items() if v[3]}
ELEMENTS["void"] = ("w", "void", "void", 1)

# The object kinds a host maps from the guest's names (`class` in gl.xml).
CLASSES = {
    "buffer": "buffers", "texture": "textures", "framebuffer": "framebuffers",
    "renderbuffer": "renderbuffers", "query": "queries", "sampler": "samplers",
    "vertex array": "vertex_arrays", "transform feedback": "transform_feedbacks",
    "program pipeline": "program_pipelines", "program": "programs", "shader": "shaders",
    "display list": None,
}

# Parameter vectors whose length a pname decides (`COMPSIZE(pname)`): the
# multi-valued ones; everything else is one value. Shared by every end, so
# what an encoder sends is what a decoder expects.
PNAME_COUNTS = {
    16: ["GL_MODELVIEW_MATRIX", "GL_PROJECTION_MATRIX", "GL_TEXTURE_MATRIX", "GL_COLOR_MATRIX",
         "GL_TRANSPOSE_MODELVIEW_MATRIX", "GL_TRANSPOSE_PROJECTION_MATRIX",
         "GL_TRANSPOSE_TEXTURE_MATRIX", "GL_TRANSPOSE_COLOR_MATRIX"],
    4: ["GL_CURRENT_COLOR", "GL_CURRENT_TEXTURE_COORDS", "GL_CURRENT_RASTER_POSITION",
        "GL_CURRENT_RASTER_COLOR", "GL_CURRENT_RASTER_TEXTURE_COORDS", "GL_CURRENT_SECONDARY_COLOR",
        "GL_FOG_COLOR", "GL_LIGHT_MODEL_AMBIENT", "GL_ACCUM_CLEAR_VALUE", "GL_COLOR_CLEAR_VALUE",
        "GL_COLOR_WRITEMASK", "GL_BLEND_COLOR", "GL_VIEWPORT", "GL_SCISSOR_BOX",
        "GL_MAP2_GRID_DOMAIN", "GL_TEXTURE_ENV_COLOR", "GL_TEXTURE_BORDER_COLOR",
        "GL_TEXTURE_SWIZZLE_RGBA", "GL_AMBIENT", "GL_DIFFUSE", "GL_SPECULAR", "GL_EMISSION",
        "GL_POSITION", "GL_AMBIENT_AND_DIFFUSE", "GL_OBJECT_PLANE", "GL_EYE_PLANE",
        "GL_CONVOLUTION_BORDER_COLOR", "GL_CONVOLUTION_FILTER_SCALE", "GL_CONVOLUTION_FILTER_BIAS",
        "GL_COLOR_TABLE_SCALE", "GL_COLOR_TABLE_BIAS", "GL_CURRENT_VERTEX_ATTRIB",
        "GL_CONSTANT_COLOR0_NV", "GL_CONSTANT_COLOR1_NV", "GL_TEXTURE_CLIPMAP_CENTER_SGIX",
        "GL_POST_CONVOLUTION_COLOR_TABLE", "GL_DEPTH_BOUNDS_EXT", "GL_COLOR"],
    3: ["GL_CURRENT_NORMAL", "GL_SPOT_DIRECTION", "GL_COLOR_INDEXES", "GL_POINT_DISTANCE_ATTENUATION",
        "GL_MAX_COMPUTE_WORK_GROUP_COUNT", "GL_MAX_COMPUTE_WORK_GROUP_SIZE"],
    2: ["GL_DEPTH_RANGE", "GL_MAX_VIEWPORT_DIMS", "GL_POINT_SIZE_RANGE", "GL_LINE_WIDTH_RANGE",
        "GL_ALIASED_POINT_SIZE_RANGE", "GL_ALIASED_LINE_WIDTH_RANGE", "GL_POLYGON_MODE",
        "GL_MAP1_GRID_DOMAIN", "GL_MAP2_GRID_SEGMENTS", "GL_VIEWPORT_BOUNDS_RANGE",
        "GL_DEPTH_BOUNDS_EXT"],
}

# Commands left to the hand-written halves on every end even where a plan
# would fit, because the encoder keeps state about them or a host must
# allocate on their behalf: names a host maps, client arrays and their
# pointers, mapping a buffer, the pixel store, strings handed back.
HAND = {
    "glBindBuffer", "glPixelStorei", "glPixelStoref", "glGetError", "glGetString", "glGetStringi",
    "glFinish", "glFlush", "glGetIntegerv", "glGetPointerv", "glGenLists", "glNewList", "glEndList",
    "glFenceSync", "glDeleteSync", "glClientWaitSync", "glWaitSync", "glIsSync", "glGetSynciv",
    "glDebugMessageCallback", "glDebugMessageCallbackARB", "glDebugMessageCallbackAMD",
    "glReadPixels", "glGetTexImage", "glArrayElement", "glInterleavedArrays",
    "glEnableClientState", "glDisableClientState", "glClientActiveTexture",
    "glEnableVertexAttribArray", "glDisableVertexAttribArray",
    "glEnableClientStateiEXT", "glDisableClientStateiEXT", "glEnableClientStateIndexedEXT",
    "glDisableClientStateIndexedEXT", "glBindVertexArray", "glLockArraysEXT", "glUnlockArraysEXT",
    "glCreateShader", "glCreateProgram", "glCreateShaderProgramv", "glCreateShaderProgramEXT",
    "glCreateShaderObjectARB", "glCreateProgramObjectARB", "glDeleteObjectARB",
    "glShaderSource", "glShaderSourceARB", "glGetShaderSource", "glGetShaderSourceARB",
}
# Commands whose untyped input is pixel data: an unpack buffer offset when
# one is bound.
PIXEL_COMMAND = re.compile(r"(Tex|Texture|MultiTex)(Sub)?Image|CompressedTex|CompressedMultiTex|"
                           r"DrawPixels|ColorTable|ColorSubTable|ConvolutionFilter|SeparableFilter")
HAND_PREFIXES = ("glDelete", "glCreate", "glMapBuffer", "glMapNamedBuffer", "glUnmap", "glFlushMapped",
                 "glGetBufferPointer", "glGetNamedBufferPointer")
# glGen<objects>: the encoders choose the names.
GEN_OBJECTS = re.compile(r"^glGen(?!erate)[A-Z]\w*")


def text(e):
    return "".join(e.itertext())


class Param:
    def __init__(self, e):
        self.name = e.find("name").text
        decl = re.sub(r"\s+", " ", text(e)).strip()
        self.decl = decl
        pt = e.find("ptype")
        self.base = pt.text if pt is not None else ("void" if "void" in decl else decl.rsplit(" ", 1)[0])
        before = decl[: decl.rfind(self.name)]
        self.ptr = before.count("*")
        self.const = bool(re.search(r"\bconst\b", before))
        self.len = e.get("len")
        self.cls = e.get("class")


class Command:
    def __init__(self, e):
        proto = e.find("proto")
        self.name = proto.find("name").text
        ret = re.sub(r"\s+", " ", text(proto)).strip()
        self.ret = ret[: ret.rfind(self.name)].strip()
        self.params = [Param(p) for p in e.findall("param")]
        a = e.find("alias")
        self.alias = a.get("name") if a is not None else None


def load():
    root = ET.parse(REGISTRY).getroot()
    commands = {}
    for e in root.find("commands"):
        c = Command(e)
        commands[c.name] = c
    # The suffixed spellings guest/gl serves as the core command although the
    # registry does not mark them aliases (tools/gen_gl_extensions.py).
    sys.path.insert(0, str(ROOT / "tools"))
    from gen_gl_extensions import CURATED_ALIASES
    for name, target in CURATED_ALIASES.items():
        commands[name].alias = target
    enums = {}
    for group in root.findall("enums"):
        for e in group.findall("enum"):
            if e.get("api") in (None, "gl"):
                enums.setdefault(e.get("name"), int(e.get("value"), 0))
    wanted = set()
    for f in root.findall("feature"):
        if f.get("api") == "gl":
            for req in f.findall("require"):
                wanted |= {c.get("name") for c in req.findall("command")}
    for ext in root.find("extensions"):
        sup = (ext.get("supported") or "").split("|")
        if not {"gl", "glcore"} & set(sup):
            continue
        for req in ext.findall("require"):
            if req.get("api") in (None, "gl"):
                wanted |= {c.get("name") for c in req.findall("command")}
    return commands, enums, wanted


class Skip(Exception):
    pass


def count_spec(cmd, p, index_of):
    """The count token for pointer parameter p, or Skip."""
    L = p.len
    names = [q.name for q in cmd.params]
    scalars = {q.name for q in cmd.params if q.ptr == 0}
    if p.base in ("GLchar", "GLcharARB") and p.const:
        if L is None or L in ("COMPSIZE(name)", "COMPSIZE()"):
            return "s"
        m = re.fullmatch(r"COMPSIZE\((\w+),(\w+)\)", L)
        if m and m.group(2) in scalars and m.group(1) == p.name:
            return f"S{index_of[m.group(2)]}"
        if L in scalars:
            return f"S{index_of[L]}"
        raise Skip(f"string length {L}")
    if L is None:
        raise Skip(f"{p.name} has no length")
    if L.isdigit():
        if int(L) == 0:
            raise Skip("zero-length array")
        return f"n{L}"
    m = re.fullmatch(r"(\w+)\s*\*\s*(\d+)", L)
    if m and m.group(1) in scalars:
        return f"p{index_of[m.group(1)]}*{m.group(2)}"
    if L in scalars:
        if p.base == "void":
            return f"z{index_of[L]}"
        return f"p{index_of[L]}"
    m = re.fullmatch(r"COMPSIZE\(([\w,]*)\)", L)
    if not m:
        raise Skip(f"length {L}")
    args = [a for a in m.group(1).split(",") if a]
    if args and args[-1] in ("pname",) and args[-1] in scalars and (len(args) == 1 or cmd.name.startswith(("glGetTexLevelParameter",))):
        return f"e{index_of[args[-1]]}"
    if len(args) == 1 and args[0] == "pname" and "pname" in scalars:
        return f"e{index_of['pname']}"
    if p.const and p.base == "void" and args and args[0] == "format" and args[1:2] == ["type"] and 3 <= len(args) <= 5:
        idx = [index_of[a] if a in index_of else "_" for a in args[2:]]
        while len(idx) < 3:
            idx.append("_")
        return "X%d,%d,%s" % (index_of["format"], index_of["type"], ",".join(str(i) for i in idx))
    if args == ["width", "height"] and p.base == "GLubyte" and cmd.name == "glBitmap":
        return f"B{index_of['width']},{index_of['height']}"
    if args == [] and cmd.name == "glPolygonStipple":
        return "T"
    if len(args) == 2 and p.const and args[1] == "type" and args[0] in scalars and p.name == "lists":
        return f"L{index_of[args[0]]},{index_of['type']}"
    if len(args) == 1 and args[0] in scalars and p.base == "void" and args[0] == "size":
        return f"z{index_of['size']}"
    # Indexed state (glGetIntegeri_v): sized as that state is.
    if args == ["target"] and not p.const and re.search(r"i_v|Indexedv", cmd.name):
        return f"e{index_of['target']}"
    # glClearBuffer*v: four for GL_COLOR, one for depth or stencil.
    if args == ["buffer"] and "buffer" in scalars:
        return f"e{index_of['buffer']}"
    # One value per uniform.
    if args == ["uniformCount", "pname"] and "uniformCount" in scalars:
        return f"p{index_of['uniformCount']}"
    # A uniform's value: as many as its type holds, which the host knows.
    if len(args) == 2 and args[1] == "location" and not p.const and args[0] in scalars:
        return f"U{index_of[args[0]]},{index_of['location']}"
    raise Skip(f"length {L}")


def offset_pointer(cmd, p):
    """A vertex, index or indirect pointer: an offset into a bound buffer, or
    client memory the encoder uploads to one first. Never carried as data."""
    if p.base != "void" or not p.const or p.ptr != 1:
        return False
    if p.name == "indirect":
        return True
    if p.name == "pointer" and re.search(r"Pointer", cmd.name):
        return p.len is None or p.len.startswith("COMPSIZE(")
    if p.name == "indices" and re.match(r"glDraw(Range)?Elements", cmd.name):
        return True
    return False


# Commands whose pointers are buffer offsets (offset_pointer): planned for
# the hosts, which take the offset; the encoders keep the client-memory case
# by hand and call these by opcode.
POINTER_COMMANDS = re.compile(r"^gl((Vertex|Color|TexCoord|Normal|SecondaryColor|FogCoord|EdgeFlag|Index)Pointer"
                              r"|VertexAttrib[IL]?Pointer|Draw(Range)?Elements\w*|Draw(Arrays|Elements)Indirect"
                              r"|MultiDraw(Arrays|Elements)Indirect)(ARB|EXT)?$")


def plan_of(cmd):
    """(plan, reason): the wire plan, or None and why not."""
    try:
        pointer_command = bool(POINTER_COMMANDS.match(cmd.name))
        if not pointer_command and (cmd.name in HAND or cmd.name.startswith(HAND_PREFIXES) or GEN_OBJECTS.match(cmd.name)):
            raise Skip("hand-written")
        ret = cmd.ret.replace("const", "").strip()
        if ret == "void":
            r = "v"
        elif ret in SCALARS and "*" not in ret:
            r = SCALARS[ret][0]
            if r == "q" and SCALARS[ret][3] is None:
                raise Skip("pointer-sized return")
        else:
            raise Skip(f"returns {cmd.ret}")
        index_of = {q.name: i for i, q in enumerate(cmd.params)}
        toks = []
        for p in cmd.params:
            if p.ptr == 0:
                if p.base not in SCALARS:
                    raise Skip(f"{p.name} is a {p.base}")
                if p.cls and p.cls not in CLASSES:
                    raise Skip(f"{p.name} is a {p.cls}")
                toks.append(SCALARS[p.base][0])
                continue
            if p.ptr != 1:
                raise Skip(f"{p.name} is a pointer to pointers")
            if p.base not in ELEMENTS:
                raise Skip(f"{p.name} points to {p.base}")
            if p.cls:
                raise Skip(f"{p.name} is an array of {p.cls} names")
            if offset_pointer(cmd, p):
                toks.append("P")
                continue
            spec = count_spec(cmd, p, index_of)
            size = ELEMENTS[p.base][3]
            if not p.const:
                if spec[0] in "XBTLsSz" and not (spec[0] == "z"):
                    raise Skip(f"output {p.name} sized {spec}")
                if p.base == "void" and spec[0] != "z":
                    raise Skip(f"untyped output {p.name}")
                toks.append(f"o{size}:{spec}")
            else:
                if spec[0] in "XBTLsSz":
                    size = 1
                # Pixel data may be an offset into the bound unpack buffer.
                pbo = spec[0] in "XBT" or (p.base == "void" and PIXEL_COMMAND.search(cmd.name))
                toks.append(f"{'j' if pbo else 'i'}{size}:{spec}")
        if "P" in toks and not pointer_command:
            raise Skip("an offset pointer in a command the encoders do not track")
        return r + " " + " ".join(toks) if toks else r, None
    except Skip as e:
        return None, str(e)


def guest_type(p):
    if p.ptr:
        base = ELEMENTS.get(p.base, ("", "void"))[1] if p.base in ELEMENTS else "void"
        return ("const " if p.const else "") + base + " *"
    return SCALARS[p.base][1]


def host_type(p):
    if p.ptr:
        base = ELEMENTS[p.base][2]
        return ("const " if p.const else "") + base + "*"
    return SCALARS[p.base][2]


def guest_ret(cmd):
    r = cmd.ret.replace("const", "").strip()
    return "void" if r == "void" else SCALARS[r][1]


def host_ret(cmd):
    r = cmd.ret.replace("const", "").strip()
    return "void" if r == "void" else SCALARS[r][2]


def guest_value(p):
    """The parameter as the uint64 the guest interpreter takes."""
    n = p.name + "_"
    if p.ptr:
        return f"(uint64_t)(uintptr_t){n}"
    t = SCALARS[p.base]
    if t[0] == "f":
        return f"(uint64_t)as_bits({n})"
    if p.base in ("GLdouble", "GLclampd"):
        return f"gexgen_dbits({n})"
    if t[0] == "q":
        return f"(uint64_t)(int64_t){n}" if t[1] in ("long", "long long") else f"(uint64_t){n}"
    if t[1] in ("int", "short", "signed char", "char"):
        return f"(uint64_t)(uint32_t)(int32_t){n}"
    return f"(uint64_t)(uint32_t){n}"


def defined_in(path):
    """C function definitions in a source file (to leave hand-written ones be)."""
    src = path.read_text() if path.exists() else ""
    names = set(re.findall(r"^[A-Za-z_][\w \*]*?\b(gl[A-Z]\w*)\s*\([^;{]*\)\s*\{", src, re.M))
    names |= set(re.findall(r"__typeof\(\w+\)\s+(gl[A-Z]\w*)", src))
    return names


def emit_guest(cmds, plans, opcodes):
    sys.path.insert(0, str(ROOT / "tools"))
    from gl_buffer_writers import mark_buffer_writer, mark_persistent_reader
    hand = set()
    for f in ("gexgl.c", "gl44-generated.inc", "gexgl-glx.inc"):
        hand |= defined_in(ROOT / "guest/gl" / f)
    out = ["/* Generated by tools/gen_gl_compat.py from the Khronos registry; do not edit.",
           " *",
           " * The Linux guest's compatibility-profile entry points: every registry",
           " * command with a wire plan that gexgl.c and gl44-generated.inc do not",
           " * define, each a call to gexgen_call (gexgl.c) with its plan. Aliases",
           " * of exported commands come from gl-aliases-generated.inc. */", ""]
    n = 0
    for name in sorted(plans):
        c = cmds[name]
        if name in hand or " P" in plans[name]:
            continue
        # An alias of a command that will be exported is left to the alias
        # file, which spells it as that command.
        if c.alias and (c.alias in hand or c.alias in plans):
            continue
        params = ", ".join(f"{guest_type(p)} {p.name}_" for p in c.params) or "void"
        vals = ", ".join(guest_value(p) for p in c.params)
        rt = guest_ret(c)
        body = f"uint64_t args__[{max(len(c.params), 1)}] = {{{vals or '0'}}};\n    "
        call = f"gexgen_call({opcodes[name]}u, \"{plans[name]}\", args__)"
        r = plans[name].split(" ")[0]
        if rt == "void":
            body += call + ";"
        elif r == "f":
            body += f"uint32_t ret__ = (uint32_t){call}; float f__; memcpy(&f__, &ret__, 4); return f__;"
        else:
            body += f"return ({rt}){call};"
        out.append(mark_persistent_reader(name, mark_buffer_writer(name, f"{rt} {name}({params}) {{\n    {body}\n}}")))
        n += 1
    out.insert(6, f"/* {n} entry points. */")
    return "\n".join(out) + "\n"


def emit_pname_counts(enums):
    lines = ["/* Generated by tools/gen_gl_compat.py; do not edit.",
             " *",
             " * How many values a pname-sized parameter vector holds (COMPSIZE(pname)",
             " * in the registry): the guest encoders, opengl32's and every host",
             " * decoder size those vectors with this one table. */",
             "#ifndef GEXGL_PNAME_COUNTS_H", "#define GEXGL_PNAME_COUNTS_H", "",
             "static inline unsigned gexgl_pname_count(unsigned pname) {", "    switch (pname) {"]
    seen = {}
    for count in sorted(PNAME_COUNTS, reverse=True):
        for e in PNAME_COUNTS[count]:
            v = enums.get(e)
            if v is None or v in seen:
                continue
            seen[v] = count
    for v, count in sorted(seen.items()):
        lines.append(f"    case 0x{v:04X}u: return {count}u;")
    lines += ["    default: return 1u;", "    }", "}", "", "#endif", ""]
    return "\n".join(lines)


def host_case(c, opcode, plan):
    toks = plan.split(" ")
    r, ptoks = toks[0], toks[1:]
    # Never inlined: each is called from one case of the decoder's switch, and
    # inlined, all 2,520 made that switch one 1.2 MB function -- slow for V8
    # to compile, and indivisible by wasm-split.
    L = [f"/* {c.name} */", f"template <typename R> __attribute__((noinline)) static bool gexgen_{opcode}(R& a) {{"]
    args = []
    post = []
    check = []
    names = [p.name + "_" for p in c.params]
    for i, (p, t) in enumerate(zip(c.params, ptoks)):
        n = names[i]
        if t == "P":
            L.append(f"    const void* {n} = reinterpret_cast<const void*>(static_cast<uintptr_t>(a.u64()));")
            args.append(n)
            continue
        if t in ("w", "f", "q"):
            ht = host_type(p)
            rd = {"w": "a.u32()", "f": "a.f32()", "q": "a.u64()"}[t]
            if p.base in ("GLdouble", "GLclampd"):
                rd = "a.f64()"
            elif t == "q":
                rd = "a.i64()" if ht in ("GLint64", "ptrdiff_t") else "a.u64()"
            elif t == "w" and ht in ("GLint", "GLsizei", "GLshort", "GLbyte"):
                rd = "a.i32()"
            if p.cls and CLASSES.get(p.cls):
                L.append(f"    const {ht} {n} = ({ht})GEXGEN_NAME({CLASSES[p.cls]}, {rd});")
            else:
                L.append(f"    const {ht} {n} = ({ht}){rd};")
            args.append(n)
            continue
        kind, rest = t[0], t[1:]
        pbo = kind == "j"
        kind = "i" if pbo else kind
        size, spec = rest.split(":", 1)
        size = int(size)
        et = ELEMENTS[p.base][2]
        if spec[0] == "n":
            count = spec[1:]
        elif spec[0] == "p":
            m = re.fullmatch(r"p(\d+)(?:\*(\d+))?", spec)
            count = f"gexgen_count({names[int(m.group(1))]}, {m.group(2) or 1})"
        elif spec[0] == "e":
            count = f"gexgl_pname_count((unsigned){names[int(spec[1:])]})"
        elif spec[0] == "z":
            count = f"gexgen_count({names[int(spec[1:])]}, 1)"
        elif spec[0] == "U":
            pi, li = spec[1:].split(",")
            count = f"uniform_components({names[int(pi)]}, {names[int(li)]})"
        else:
            count = None
        if kind == "o":
            check.append(f"    const size_t {n}count = {count};")
            check.append(f"    std::vector<uint64_t> {n}store(gexgen_words({n}count < 16 ? 16 : {n}count, {size}), 0);")
            check.append(f"    {et}* {n} = reinterpret_cast<{et}*>({n}store.data());")
            post.append(f"    put_blob({n}, (uint32_t)({n}count * {size}));")
            args.append(n)
            continue
        L.append(f"    gexgen_raw {n}raw; std::vector<uint64_t> {n}store;")
        L.append(f"    if (!gexgen_take(a, {n}raw, {'true' if pbo else 'false'})) return false;")
        if spec[0] in "sS":
            check.append(f"    const char* {n}s = nullptr;")
            check.append(f"    if (!gexgen_check_string({n}raw, {n}store, &{n}s)) {{ a.ok = false; return false; }}")
            check.append(f"    const {et}* {n} = reinterpret_cast<const {et}*>({n}s);")
            args.append(n)
            continue
        check.append(f"    const void* {n}v = nullptr;")
        if spec[0] == "X":
            f_, t_, w_, h_, d_ = spec[1:].split(",")
            dim = lambda x, dflt: names[int(x)] if x != "_" else dflt
            test = (f"gexgen_check_pixels({n}raw, {n}store, &{n}v, {names[int(f_)]}, {names[int(t_)]}, "
                    f"{dim(w_, '1')}, {dim(h_, '1')}, {dim(d_, '-1')})")
        else:
            if spec[0] == "B":
                w_, h_ = spec[1:].split(",")
                bytes_ = f"gexgen_bitmap_bytes({names[int(w_)]}, {names[int(h_)]})"
            elif spec[0] == "T":
                bytes_ = "128"
            elif spec[0] == "L":
                ni, ti = spec[1:].split(",")
                bytes_ = f"gexgen_count({names[int(ni)]}, (long long)gexgen_type_bytes({names[int(ti)]}))"
            else:
                bytes_ = f"{count} * {size}"
            test = f"gexgen_check({n}raw, {n}store, &{n}v, {bytes_})"
        check.append(f"    if (!{test}) {{ a.ok = false; return false; }}")
        check.append(f"    const {et}* {n} = reinterpret_cast<const {et}*>({n}v);")
        args.append(n)
    L.append("    if (!a.ok) return false;")
    L += check
    ftype = f"{host_ret(c)} (*)({', '.join(host_type(p) for p in c.params)})"
    alias = f'"{c.alias}"' if c.alias else "nullptr"
    L.append(f"    using F__ = {ftype};")
    L.append(f"    static F__ f__ = reinterpret_cast<F__>(gexgen_proc(\"{c.name}\", {alias}));")
    call = f"f__({', '.join(args)})"
    missing = "put_u32(0); " if r in ("w", "f") else "put_u64(0); " if r == "q" else ""
    L.append(f"    if (!f__) {{ gexgen_missing(\"{c.name}\"); {missing}" +
             "".join(f"put_blob(nullptr, 0); " for t in ptoks if t[0] == "o") + "return true; }")
    if r == "v":
        L.append(f"    {call};")
    elif r == "f":
        L.append(f"    const float ret__ = {call}; uint32_t bits__; memcpy(&bits__, &ret__, 4); put_u32(bits__);")
    elif r == "q":
        L.append(f"    put_u64((uint64_t){call});")
    else:
        L.append(f"    put_u32((uint32_t){call});")
    L += post
    L.append("    return true;")
    L.append("}")
    return "\n".join(L)


def emit_host(cmds, plans, opcodes):
    """The decoders (a function per command, which a compiler takes far
    better than one switch of thousands of cases) and the cases."""
    out = ["/* Generated by tools/gen_gl_compat.py from the Khronos registry; do not edit.",
           " *",
           " * The host half of every planned command: decode, map the guest's object",
           " * names (GEXGEN_NAME), call the host's GL through gexgen_proc, reply;",
           " * false for a malformed command. The includer defines the Reader,",
           " * put_u32/put_u64/put_blob, GEXGEN_NAME, uniform_components,",
           " * gexgen_proc/gexgen_missing, and includes web/mesa/gexgl-compat-host.h;",
           " * the cases (gl-compat-generated-cases.inc) go in its switch. */"]
    cases = ["/* Generated by tools/gen_gl_compat.py; do not edit. The decoders are",
             " * gl-compat-generated-decoders.inc. */"]
    for name in sorted(plans):
        out.append(host_case(cmds[name], opcodes[name], plans[name]))
        cases.append(f"case {opcodes[name]}: if (!gexgen_{opcodes[name]}(a)) return false; break;")
    return "\n".join(out) + "\n", "\n".join(cases) + "\n"


def emit_rust_table(plans, opcodes):
    out = ["// Generated by tools/gen_gl_compat.py; do not edit.", "",
           "/// The first opcode of the generated compatibility-profile commands.",
           f"pub const COMPAT_BASE: u32 = {COMPAT_BASE};",
           "/// Every planned command: name, opcode and plan.",
           "pub const PLANS: &[(&str, u32, &str)] = &["]
    out += [f'    ("{n}", {opcodes[n]}, "{plans[n]}"),' for n in sorted(plans)]
    out += ["];", ""]
    return "\n".join(out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true", help="fail if a checked-in output differs")
    ap.add_argument("--report", action="store_true", help="list the commands left out, and why")
    args = ap.parse_args()
    cmds, enums, wanted = load()
    allnames = sorted(cmds)
    opcodes = {n: COMPAT_BASE + allnames.index(n) for n in allnames}
    plans, skipped = {}, {}
    for n in sorted(wanted):
        p, why = plan_of(cmds[n])
        if p is None:
            skipped[n] = why
        else:
            plans[n] = p
    pname_c = emit_pname_counts(enums)
    host_decoders, host_cases = emit_host(cmds, plans, opcodes)
    outputs = {
        ROOT / "guest/gl/gl-compat-generated.inc": emit_guest(cmds, plans, opcodes),
        ROOT / "guest/gl/gl-pname-counts.h": pname_c,
        ROOT / "crates/gex-gl/src/compat_plans.rs": emit_rust_table(plans, opcodes),
        ROOT / "web/mesa/gl-compat-generated-decoders.inc": host_decoders,
        ROOT / "web/mesa/gl-compat-generated-cases.inc": host_cases,
    }
    if args.report:
        for n, why in sorted(skipped.items()):
            print(f"{n}: {why}")
        print(f"{len(plans)} planned, {len(skipped)} left out", file=sys.stderr)
    if args.check:
        stale = [str(p.relative_to(ROOT)) for p, v in outputs.items() if not p.is_file() or p.read_text() != v]
        if stale:
            ap.error("stale generated files: " + ", ".join(stale))
        return 0
    for p, v in outputs.items():
        p.write_text(v)
    return 0


if __name__ == "__main__":
    sys.exit(main())
