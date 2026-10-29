#!/bin/sh
# Build gex-embed: a static library for an emscripten host to link (android-wasm's dotnet.native.wasm).
#
# The objects must agree with the host's on every wasm feature, because they are linked into one module:
#   atomics, bulk-memory    the host's memory is shared (pthreads), and a shared memory needs both
#   mutable-globals         __stack_pointer and the TLS globals, per thread
#   sign-ext, nontrapping-fptoint, simd128
#                           what the host's emcc enables (-mnontrapping-fptoint, -msimd128)
# and nothing else. `target-cpu=mvp` turns off the LLVM defaults (reference-types, multivalue, ...) that the frozen
# emsdk's wasm-ld (LLVM 19) and binaryen might not take. The standard library is rebuilt the same way
# (-Z build-std), which needs rust-src: 1.96.0 (the workspace pin) has none installed here, so 1.97.1 (LLVM 22, the
# closest to the host's 19) by default, with RUSTC_BOOTSTRAP for the -Z flag.
#
# One feature can't be turned off: LLVM 22 implies `bulk-memory-opt` (memory.copy/fill alone) from `bulk-memory`,
# and writes it into every object's target_features section. wasm-ld 19 links that fine, but emcc then passes
# every feature of the linked module to binaryen as --enable-<name>, and the frozen binaryen has no
# --enable-bulk-memory-opt. So the archive's target_features sections are rewritten to the list below, which is
# the same code: bulk-memory-opt is a subset of bulk-memory.
set -e
cd "$(dirname "$0")/../.."
TOOLCHAIN=${GEX_EMBED_TOOLCHAIN:-1.97.1}
FEATURES=+atomics,+bulk-memory,+mutable-globals,+sign-ext,+nontrapping-fptoint,+simd128
SYSROOT=$(rustc +"$TOOLCHAIN" --print sysroot)
REMAP="--remap-path-prefix=$(pwd)=/gex --remap-path-prefix=$SYSROOT=/rust --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo"
RUSTC_BOOTSTRAP=1 \
RUSTFLAGS="-C target-cpu=mvp -C target-feature=$FEATURES -C panic=abort $REMAP" \
CARGO_PROFILE_RELEASE_DEBUG=${GEX_EMBED_DEBUG:-0} \
    cargo +"$TOOLCHAIN" build -p gex-embed --target wasm32-unknown-unknown --release \
        -Z build-std=std,panic_abort --target-dir target/embed
LIB=target/embed/wasm32-unknown-unknown/release/libgex_embed.a
TF=target/embed/target_features.bin
python3 -I - "$FEATURES" "$TF" <<'PY'
import sys
def leb(n):
    o = bytearray()
    while True:
        b, n = n & 0x7f, n >> 7
        o.append(b | (0x80 if n else 0))
        if not n:
            return bytes(o)
feats = [f[1:] for f in sys.argv[1].split(',')]
open(sys.argv[2], 'wb').write(leb(len(feats)) + b''.join(b'+' + leb(len(f)) + f.encode() for f in feats))
PY
${LLVM_OBJCOPY:-llvm-objcopy} --remove-section=target_features --add-section=target_features="$TF" "$LIB"
echo "shims/embed: $LIB ($(stat -c%s "$LIB") bytes)"
