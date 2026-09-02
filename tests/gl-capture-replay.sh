#!/bin/sh
# Record a guest's GL command stream, replay it, and reduce it.
#
#   tests/gl-capture-replay.sh
#
# The three steps are one gate because they are one workflow, and because each
# only means something with the others: a recording nothing can replay is a
# file, and a replay nothing can shrink cannot name which command is at fault.
#
# What is asserted here is the *transport* invariant -- every batch a guest
# emits is decodable by the backend that will execute it -- not that the
# backend renders it correctly. GL errors are allowed through on purpose: the
# replay backend still has gaps, and this gate must keep working while they are
# closed rather than having to be disabled for each one.
#
# Skips when the aarch64 toolchain or the Mesa Wasm build is absent.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
WORK=${GEX_GL_CAPTURE_WORK:-${TMPDIR:-/tmp}/gex-gl-capture}
MESA_OUT=${GEX_MESA_WASM_OUT:-${TMPDIR:-/tmp}/gex-mesa-wasm}
REPLAY=$MESA_OUT/build/src/gallium/targets/gex-wasm/gex-mesa-replay.js
TOOLCHAIN=${GEX_TOOLCHAIN:-$HOME/toolchains/aarch64-linux-musl-cross}
CC=${GEX_CC:-$TOOLCHAIN/bin/aarch64-linux-musl-gcc}
GEXC=${GEXC:-$ROOT/target/release/gexc}

[ -x "$CC" ] || { echo "skip: no aarch64 toolchain at $CC"; exit 0; }
[ -f "$REPLAY" ] || { echo "skip: no Mesa replay module at $REPLAY (web/build-mesa-wasm.sh)"; exit 0; }
[ -x "$GEXC" ] || { echo "skip: no gexc at $GEXC (cargo build --release -p gexc)"; exit 0; }
[ -d /usr/include/GLES2 ] || { echo "skip: no GLES2 headers to build the guest program against"; exit 0; }

rm -rf "$WORK"
mkdir -p "$WORK/root/lib" "$WORK/include"

# The guest's own GL driver: the thing whose output is being recorded.
GEX_CC="$CC" "$ROOT/guest/gl/build.sh" "$WORK/driver" >/dev/null
cp -r /usr/include/EGL /usr/include/GLES2 /usr/include/KHR "$WORK/include/"

# A sysroot with just the loader and that driver, so the guest resolves its
# libraries the way it would in a real root rather than against the host's.
cp "$TOOLCHAIN/aarch64-linux-musl/lib/libc.so" "$WORK/root/lib/ld-musl-aarch64.so.1"
cp "$WORK/driver/libGLESv2.so.2" "$WORK/driver/libEGL.so.1" "$WORK/root/lib/"

"$CC" -O2 -w -o "$WORK/root/gears" "$ROOT/tests/integration/programs/gles-gears.c" \
    -I "$WORK/include" -L "$WORK/driver" -lEGL -lGLESv2 -lm

TRACE=$WORK/gears.gexgltr
GEX_GL_CAPTURE=$TRACE EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 \
    timeout 300 "$GEXC" run --gl --sysroot "$WORK/root" "$WORK/root/gears" \
    >"$WORK/guest.log" 2>&1 || {
        echo "FAIL gl-capture-replay: the guest program did not finish"
        tail -20 "$WORK/guest.log" | sed 's/^/     /'
        exit 1
    }
[ -s "$TRACE" ] || { echo "FAIL gl-capture-replay: nothing was recorded to $TRACE"; exit 1; }

# Recording is not sampling: a trace that dropped batches would replay as a
# different program, so check the count against what the guest reported.
frames=$(sed -n 's/^frames //p' "$WORK/guest.log" | head -1)
node -e '
import(process.argv[3]).then(async (trace) => {
  const { readFileSync } = await import("node:fs");
  const t = trace.parseTrace(new Uint8Array(readFileSync(process.argv[1])));
  const frames = Number(process.argv[2]);
  const commands = trace.flatten(t).length;
  if (t.batches.length < frames)
    throw new Error(`recorded ${t.batches.length} batches for ${frames} frames`);
  console.log(`     recorded ${t.batches.length} batches, ${commands} commands, ` +
              `${t.info.width}x${t.info.height}, protocol ${t.info.protocolVersion}`);
});' "$TRACE" "${frames:-1}" "$ROOT/tests/gl-trace.mjs"

# Every batch decodes. GL errors are the backend's business, not the wire's.
node "$ROOT/tests/gl-replay.mjs" "$TRACE" "$REPLAY" --allow-gl-errors >"$WORK/replay.log" 2>&1 || {
    echo "FAIL gl-capture-replay: the recorded stream did not replay"
    grep -v "Heap resize" "$WORK/replay.log" | tail -20 | sed 's/^/     /'
    exit 1
}
grep -q "^PASS gl-replay" "$WORK/replay.log"

# Reduction, against an oracle that does not depend on any backend defect
# staying unfixed: a candidate "reproduces" while it still contains a shader
# compile. The answer is therefore known -- the pinned commands plus exactly
# one COMPILE_SHADER -- which is what makes this a test of the reducer rather
# than of whatever is currently broken.
cat > "$WORK/oracle.mjs" <<ORACLE
import { readFileSync } from "node:fs";
// Absolute: an oracle is run from wherever the reducer happens to live, and a
// relative import that failed to resolve would exit non-zero for every
// candidate -- which reads exactly like "everything still reproduces".
import { flatten, parseTrace } from "$ROOT/tests/gl-trace.mjs";
const trace = parseTrace(new Uint8Array(readFileSync(process.argv[2])));
process.exit(flatten(trace).some((c) => c.op === 50) ? 1 : 0);
ORACLE

node "$ROOT/tests/gl-reduce.mjs" "$TRACE" --out "$WORK/min.gexgltr" \
    --oracle "node $WORK/oracle.mjs {}" >"$WORK/reduce.log" 2>&1 || {
        echo "FAIL gl-capture-replay: reduction failed"
        tail -20 "$WORK/reduce.log" | sed 's/^/     /'
        exit 1
    }
sed 's/^/     /' "$WORK/reduce.log"

node -e '
import(process.argv[2]).then(async (trace) => {
  const { readFileSync } = await import("node:fs");
  const t = trace.parseTrace(new Uint8Array(readFileSync(process.argv[1])));
  const ops = trace.flatten(t).map((c) => c.op);
  const compiles = ops.filter((op) => op === 50).length;
  if (compiles !== 1)
    throw new Error(`reduction kept ${compiles} shader compiles, wanted exactly 1`);
  // CONTEXT_CREATE and SURFACE_RESIZE are pinned; nothing else may survive.
  const extra = ops.filter((op) => op !== 50 && op !== 1 && op !== 2);
  if (extra.length)
    throw new Error(`reduction kept unrelated commands: ${extra.join(",")}`);
})' "$WORK/min.gexgltr" "$ROOT/tests/gl-trace.mjs"

# The reduced traces kept from earlier reductions. Each one is a defect that
# was found, fixed, and must not come back; they replay in about a second, so
# there is no reason for them not to be part of this gate.
for trace in "$ROOT"/tests/gl-traces/*.gexgltr; do
    # An unmatched glob arrives as the pattern itself.
    [ -f "$trace" ] || continue
    name=$(basename "$trace" .gexgltr)
    api=es
    case "$name" in
        default-framebuffer-clear) api=core ;;
    esac
    node "$ROOT/tests/gl-replay.mjs" "$trace" "$REPLAY" --api "$api" --quiet \
        >"$WORK/$name.log" 2>&1 || {
            echo "FAIL gl-capture-replay: $name no longer replays cleanly"
            grep -v "Heap resize" "$WORK/$name.log" | tail -10 | sed 's/^/     /'
            exit 1
        }
    echo "     replayed $name (--api $api)"
done

echo "ok   gl-capture-replay"
