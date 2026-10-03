#!/bin/sh
# Build the guest's GL driver: libGLESv2.so.2 and libEGL.so.1.
#
# Those sonames are the point. An application links against them by name, so
# putting these where the guest's loader looks is what makes an unmodified
# binary use the host's GL. Nothing about the application changes.
#
# One object provides both, because the split between EGL and GLES is a
# packaging convention rather than a boundary in the code, and a guest that
# loads one always loads the other.
set -eu
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
OUT="${1:-$HERE/out}"
CC="${GEX_CC:-$HOME/toolchains/aarch64-linux-musl-cross/bin/aarch64-linux-musl-gcc}"

# A name as well as a path. `GEX_CC=aarch64-linux-gnu-gcc` used to be reported
# as a missing compiler and *skipped* -- exit zero, no output, no libraries --
# so a caller that trusted the exit status went on to copy files that were
# never built.
if [ ! -x "$CC" ]; then
    resolved=$(command -v "$CC" 2>/dev/null || true)
    [ -z "$resolved" ] || CC=$resolved
fi
[ -x "$CC" ] || { echo "skip: no $CC"; exit 0; }

# Generate every registry-backed entry point before compiling the transport.
# This deliberately fails closed if a pointer-shaped command has no explicit
# wire policy in tools/gen_gl44_transport.py.
python3 "$ROOT/tools/gen_gl44_transport.py"
# And every other registry command with a wire plan: the guest's entry
# points, opengl32's plans and the hosts' decoder (tools/gen_gl_compat.py).
python3 "$ROOT/tools/gen_gl_compat.py"

# The opcodes come from the Rust side, so the two cannot drift.
python3 - "$ROOT" "$HERE" <<'PYEOF'
import re, sys
root, here = sys.argv[1], sys.argv[2]
src = open(f'{root}/crates/gex-gl/src/proto.rs').read()
body = src[src.index('pub mod op {'):]
body = body[:body.index('\n}')]
out = ["/* Opcodes for the gex graphics device.",
       " *",
       " * Generated from crates/gex-gl/src/proto.rs by guest/gl/build.sh. The two",
       " * halves of the protocol have to agree exactly, and the way to be sure of",
       " * that is for one of them not to be written by hand.",
       " */",
       "#ifndef GEXGL_H",
       "#define GEXGL_H",
       ""]
for m in re.finditer(r'pub const ([A-Z_0-9]+): u32 = (\d+);', body):
    out.append(f"#define GEXGL_{m.group(1)} {m.group(2)}")
# The small enumerations beside the opcodes -- who a surface belongs to, which
# API a context is -- for the same reason: written twice is written
# differently.
for name in ('surface', 'client_api'):
    marker = f'pub mod {name} {{'
    if marker not in src:
        continue
    block = src[src.index(marker):]
    block = block[:block.index('\n}')]
    out.append("")
    for m in re.finditer(r'pub const ([A-Z_0-9]+): u32 = (\d+);', block):
        out.append(f"#define GEXGL_{name.upper()}_{m.group(1)} {m.group(2)}")

magic = re.search(r'pub const MAGIC: u32 = (0x[0-9a-fA-F_]+);', src).group(1)
protocol_version = re.search(r'pub const PROTOCOL_VERSION: u32 = (\d+);', src).group(1)
out += ["", f"#define GEXGL_MAGIC {magic.replace('_','')}u",
        f"#define GEXGL_PROTOCOL_VERSION {protocol_version}u"]
dev = open(f'{root}/shims/native/src/gl.rs').read()
sub = re.search(r'pub const GEX_GL_SUBMIT: u64 = (0x[0-9a-fA-F_]+);', dev).group(1)
out += [f"#define GEXGL_SUBMIT {sub.replace('_','')}u"]
browser = open(f'{root}/shims/browser/src/gl.rs').read()
surface_ioctl = re.search(r'pub const GEX_GL_SURFACE: u64 = (0x[0-9a-fA-F_]+);', browser)
if surface_ioctl:
    out += [f"#define GEXGL_SURFACE {surface_ioctl.group(1).replace('_','')}u"]
out += ["", "#endif", ""]
open(f'{here}/gexgl.h', 'w').write("\n".join(out))

# And the same for the browser, whose executor is JavaScript. Three languages
# now agree about these numbers because two of them are generated.
js = ["// Opcodes for the gex graphics device.",
      "//",
      "// Generated from crates/gex-gl/src/proto.rs by guest/gl/build.sh.",
      "// Do not edit: the guest, the native host and this must agree exactly.",
      "", "export const op = {"]
for m in re.finditer(r'pub const ([A-Z_0-9]+): u32 = (\d+);', body):
    js.append(f"    {m.group(1)}: {m.group(2)},")
js += ["};", "",
       f"export const MAGIC = {magic.replace('_','')};",
       f"export const PROTOCOL_VERSION = {protocol_version};",
       "export const HEADER_BYTES = 32;", ""]
errs = src[src.index('pub mod error {'):]
errs = errs[:errs.index('\n}')]
js += ["export const error = {"]
for m in re.finditer(r'pub const ([A-Z_0-9]+): u32 = (\d+);', errs):
    js.append(f"    {m.group(1)}: {m.group(2)},")
js += ["};", ""]
import os
webdir = f'{root}/web/src'
if os.path.isdir(webdir):
    open(f'{webdir}/gl-proto.js', 'w').write("\n".join(js))
PYEOF

mkdir -p "$OUT"
# The extension allowlist is derived from this object's own exported symbols
# -- an extension is only claimable when every command the registry says it
# requires is actually here -- so it can only be generated after a link, and
# a change to it needs one more. In steady state the second link never runs.
INCLUDE="$HERE/gl-extensions-generated.inc"
ALIASES="$HERE/gl-aliases-generated.inc"
[ -f "$INCLUDE" ] || printf '%s\n' \
    'static const char *const gexgl_claimable_extensions[] = {0};' > "$INCLUDE"
[ -f "$ALIASES" ] || : > "$ALIASES"
link_guest_gl() {
    $CC -shared -fPIC -O2 -Wall -o "$OUT/libGLESv2.so.2" \
        -Wl,-soname,libGLESv2.so.2 "$HERE/gexgl.c" -ldl
}
link_guest_gl
if [ -n "$(python3 "$ROOT/tools/gen_gl_extensions.py" \
        --library "$OUT/libGLESv2.so.2" --output "$INCLUDE" \
        --aliases "$ALIASES")" ]; then
    link_guest_gl
fi
# EGL is the same object under its own name, so a loader that opens either
# finds every symbol it needs.
cp "$OUT/libGLESv2.so.2" "$OUT/libEGL.so.1"
# And libGL.so.1, the same object again: GLX is how a traditional X program
# asks for a context, and without this file that whole class of application
# cannot start. It is a separate soname rather than a symlink because a loader
# that opens both must see two libraries, not one.
cp "$OUT/libGLESv2.so.2" "$OUT/libGL.so.1"
ln -sf libGL.so.1 "$OUT/libGL.so"
ln -sf libGLESv2.so.2 "$OUT/libGLESv2.so"
ln -sf libEGL.so.1 "$OUT/libEGL.so"
echo "$OUT: $(ls "$OUT" | tr '\n' ' ')"
