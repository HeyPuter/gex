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

[ -x "$CC" ] || { echo "skip: no $CC"; exit 0; }

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
magic = re.search(r'pub const MAGIC: u32 = (0x[0-9a-fA-F_]+);', src).group(1)
out += ["", f"#define GEXGL_MAGIC {magic.replace('_','')}u"]
dev = open(f'{root}/shims/native/src/gl.rs').read()
sub = re.search(r'pub const GEX_GL_SUBMIT: u64 = (0x[0-9a-fA-F_]+);', dev).group(1)
out += [f"#define GEXGL_SUBMIT {sub.replace('_','')}u"]
out += ["", "#endif", ""]
open(f'{here}/gexgl.h', 'w').write("\n".join(out))
PYEOF

mkdir -p "$OUT"
$CC -shared -fPIC -O2 -Wall -o "$OUT/libGLESv2.so.2" \
    -Wl,-soname,libGLESv2.so.2 "$HERE/gexgl.c"
# EGL is the same object under its own name, so a loader that opens either
# finds every symbol it needs.
cp "$OUT/libGLESv2.so.2" "$OUT/libEGL.so.1"
ln -sf libGLESv2.so.2 "$OUT/libGLESv2.so"
ln -sf libEGL.so.1 "$OUT/libEGL.so"
echo "$OUT: $(ls "$OUT" | tr '\n' ' ')"
