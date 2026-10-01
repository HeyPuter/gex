#!/usr/bin/env python3
"""Generate crates/gex-kernel/src/nt/opengl/table.rs from Wine's opengl32.

    tools/nt-gen-opengl.py WINE_SRC > crates/gex-kernel/src/nt/opengl/table.rs

Reads dlls/opengl32/unixlib.h (the unix call numbering and each call's
params struct) and include/wine/wgl.h (the extension list behind
`enum opengl_extension`). Each params struct becomes a signature string of
field kinds, from which the NT layer computes the layout for either ABI;
the layouts computed here are checked against MinGW's offsetof() for x86-64
and i686 before anything is written, so they are Wine's own.
"""

import re
import subprocess
import sys
import tempfile
import os

W = sys.argv[1]
UNIXLIB = os.path.join(W, "dlls/opengl32/unixlib.h")
WGL = os.path.join(W, "include/wine/wgl.h")

src = open(UNIXLIB).read()

# Field kinds: b 1 byte, h 2 bytes, i 4 bytes, f float, d double,
# q 8-byte integer, p pointer-sized (pointers, handles, GLintptr).
PTR_TYPES = {
    "TEB", "HDC", "HGLRC", "HPBUFFERARB", "GLsync", "GLeglImageOES", "GLintptr", "GLsizeiptr",
    "GLintptrARB", "GLsizeiptrARB", "GLvdpauSurfaceNV", "GLDEBUGPROC", "GLDEBUGPROCARB",
    "GLDEBUGPROCAMD", "GLDEBUGPROCKHR", "GLVULKANPROCNV", "GLeglClientBufferEXT",
}
KINDS = {
    "GLdouble": "d", "GLclampd": "d",
    "GLint64": "q", "GLuint64": "q", "GLint64EXT": "q", "GLuint64EXT": "q",
    "GLfloat": "f", "GLclampf": "f",
    "GLshort": "h", "GLushort": "h", "GLhalfNV": "h",
    "GLbyte": "b", "GLubyte": "b", "GLboolean": "b", "GLchar": "b", "GLcharARB": "b", "BOOLEAN": "b",
    "GLenum": "i", "GLuint": "i", "GLint": "i", "GLsizei": "i", "GLfixed": "i", "GLbitfield": "i",
    "BOOL": "i", "int": "i", "UINT32": "i", "unsigned int": "i", "UINT": "i", "GLhandleARB": "i",
    "GLclampx": "i", "UINT64": "q",
}


def kind(ctype):
    t = ctype.replace("const", "").strip()
    if "*" in t:
        return "p"
    t = " ".join(t.split())
    if t in PTR_TYPES or t.startswith("struct "):
        return "p"
    if t in KINDS:
        return KINDS[t]
    raise SystemExit(f"unknown type {ctype!r}")


structs = {}
for m in re.finditer(r"^struct (\w+)_params\n\{\n(.*?)\n\};", src, re.S | re.M):
    if m.group(1) == "gl_debug_message_callback":
        continue  # a callback's params, not a unix call's
    fields = []
    for line in m.group(2).splitlines():
        line = line.strip().rstrip(";")
        if not line:
            continue
        fm = re.match(r"(.*?)(\w+)$", line)
        fields.append((fm.group(2), kind(fm.group(1))))
    structs[m.group(1)] = fields

enum = re.search(r"enum unix_funcs\n\{\n(.*?)\n\};", src, re.S).group(1)
names = [l.strip().rstrip(",") for l in enum.splitlines() if l.strip()]
assert names[-1] == "funcs_count"
names = [n[len("unix_"):] for n in names[:-1]]

SIZE = {"b": 1, "h": 2, "i": 4, "f": 4, "d": 8, "q": 8}


def layout(fields, ptr):
    off, out, align = 0, [], 1
    for _, k in fields:
        s = ptr if k == "p" else SIZE[k]
        off = (off + s - 1) // s * s
        out.append(off)
        off += s
        align = max(align, s)
    return out


# Check the computed layouts against the compilers'.
checks = []
for n in names:
    if n not in structs:
        continue
    for ptr, arch in ((8, "x64"), (4, "x86")):
        for (f, _), o in zip(structs[n], layout(structs[n], ptr)):
            checks.append((arch, f"_Static_assert(offsetof(struct {n}_params, {f}) == {o}, \"{n}.{f}\");"))

exts_src = open(WGL).read()
n_ext = len(re.findall(r"USE_GL_EXT\(", re.search(r"#define ALL_GL_EXTS \\\n(.*?)\n\n", exts_src, re.S).group(1))) + \
    len(re.findall(r"USE_GL_EXT\(", re.search(r"#define ALL_WGL_EXTS \\\n(.*?)\n\n", exts_src, re.S).group(1)))
# struct opengl_client_context (wine/opengl_driver.h): the same in both ABIs.
CLIENT = [("obj", 0), ("unix_handle", 8), ("unix_funcs", 16), ("current_tid", 24), ("last_error", 28),
          ("context_flags", 32), ("profile_mask", 36), ("major_version", 40), ("minor_version", 44),
          ("broken_sharing", 48), ("extensions", 52)]
count_off = (52 + n_ext + 3) // 4 * 4
CLIENT += [("extension_count", count_off), ("extension_array", count_off + 4)]
for arch in ("x64", "x86"):
    for f, o in CLIENT:
        checks.append((arch, f"_Static_assert(offsetof(struct opengl_client_context, {f}) == {o}, \"client.{f}\");"))

with tempfile.TemporaryDirectory() as tmp:
    for arch, cc in (("x64", "x86_64-w64-mingw32-gcc"), ("x86", "i686-w64-mingw32-gcc")):
        c = os.path.join(tmp, f"check_{arch}.c")
        with open(c, "w") as fh:
            fh.write("#include <stddef.h>\n#include \"unixlib.h\"\n#include \"wine/opengl_driver.h\"\n")
            fh.write("\n".join(line for a, line in checks if a == arch))
            fh.write("\n")
        r = subprocess.run(
            [cc, "-fsyntax-only", "-DWINE_UNIX_LIB_UNUSED", "-I", os.path.join(W, "dlls/opengl32"),
             "-I", os.path.join(W, "include"), "-D__WINESRC__", c],
            capture_output=True, text=True)
        if r.returncode != 0:
            sys.stderr.write(r.stderr[:4000])
            raise SystemExit(f"layout check failed for {arch}")

exts = re.search(r"#define ALL_GL_EXTS \\\n(.*?)\n\n", open(WGL).read(), re.S).group(1)
wexts = re.search(r"#define ALL_WGL_EXTS \\\n(.*?)\n\n", open(WGL).read(), re.S).group(1)
ext_names = re.findall(r"USE_GL_EXT\((\w+)\)", exts) + re.findall(r"USE_GL_EXT\((\w+)\)", wexts)

out = []
out.append("// Generated by tools/nt-gen-opengl.py from Wine's opengl32/unixlib.h and wine/wgl.h;")
out.append("// do not edit.")
out.append("")
out.append("/// opengl32's unix calls, in `enum unix_funcs` order: the name and the")
out.append("/// params struct's field kinds (b/h/i 1/2/4-byte integer, f float, d double,")
out.append("/// q 8-byte integer, p pointer-sized), `TEB *teb` first where there is one.")
out.append("pub const FUNCS: &[(&str, &str)] = &[")
for n in names:
    sig = "".join(k for _, k in structs.get(n, []))
    out.append(f'    ("{n}", "{sig}"),')
out.append("];")
out.append("")
out.append("/// `enum opengl_extension`: GL extensions, then WGL ones.")
out.append("pub const EXTENSIONS: &[&str] = &[")
for e in ext_names:
    out.append(f'    "{e}",')
out.append("];")
out.append("")
out.append(f"pub const WGL_FIRST_EXTENSION: usize = {len(re.findall(r'USE_GL_EXT', exts))};")
out.append("")
out.append("/// `struct opengl_client_context` field offsets (the same in both ABIs).")
out.append("pub mod client {")
for f, o in CLIENT:
    out.append(f"    pub const {f.upper()}: u64 = {o:#x};")
out.append("}")
print("\n".join(out))
