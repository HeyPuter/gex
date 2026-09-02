// Replay a recorded guest GL command stream into a backend, deterministically.
//
//   node tests/gl-replay.mjs TRACE MODULE.js [options]
//
// MODULE.js is an Emscripten module exporting the wire decoder's entry points
// (`gex-mesa-replay`, built by web/build-mesa-wasm.sh, is the Mesa/softpipe
// one). The point of replaying rather than re-running the workload is that the
// bytes are fixed: two backends given this file execute the same stream, and a
// reduction of it is still the same experiment.
//
// A batch "fails" when the decoder rejects it, or -- unless --allow-gl-errors
// -- when GL reports an error afterwards. The error is asked for with an
// injected GET_ERROR, so it is attributed to the batch that caused it instead
// of surfacing many batches later.
//
// Exit status is 0 when the whole trace replayed cleanly and 1 when it did
// not, which is what makes this usable as a reduction oracle.

import { readFileSync, writeFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { parseTrace, command, op, Words } from "./gl-trace.mjs";

const argv = process.argv.slice(2);
const positional = [];
const flags = new Map();
for (let i = 0; i < argv.length; i++) {
  const arg = argv[i];
  if (!arg.startsWith("--")) {
    positional.push(arg);
    continue;
  }
  const [name, inline] = arg.slice(2).split("=", 2);
  const wantsValue = ["width", "height", "stop-after", "api", "gl-major",
                      "gl-minor", "dump-surface"].includes(name);
  flags.set(name, inline ?? (wantsValue ? argv[++i] : "1"));
}

const [tracePath, modulePath] = positional;
if (!tracePath || !modulePath) {
  console.error("usage: gl-replay.mjs TRACE MODULE.js [--api es|core] [--readback]");
  console.error("                     [--allow-gl-errors] [--width N] [--height N]");
  console.error("                     [--stop-after N] [--quiet]");
  console.error("                     [--dump-surface PATH]");
  process.exit(2);
}

const quiet = flags.has("quiet");
const allowGlErrors = flags.has("allow-gl-errors");
const wantReadback = flags.has("readback");
/* Where to write the surface itself, for tools/surface-diff.py. The digest
 * and mean below are summaries, and a summary cannot tell a flipped image
 * from a correct one -- their means are identical. Two backends can only be
 * held to the same picture by comparing the pictures. */
const dumpSurface = flags.get("dump-surface");
const stopAfter = flags.has("stop-after") ? Number(flags.get("stop-after")) : Infinity;

const trace = parseTrace(new Uint8Array(readFileSync(tracePath)));
const width = Number(flags.get("width") ?? trace.info.width) || 64;
const height = Number(flags.get("height") ?? trace.info.height) || 64;

const { default: createModule } = await import(pathToFileURL(modulePath));
const backend = await createModule();

// The wire does not say which client API the guest bound, and a GLES guest's
// shader source will not compile in a core profile. Backends that can be told
// are told; ones that cannot ignore this.
const api = flags.get("api") ?? "es";
if (!["es", "core"].includes(api)) {
  console.error(`gl-replay: --api must be es or core, not ${api}`);
  process.exit(2);
}
// Zero means the backend's own default for the selected API. Passing an
// explicit version is how two backends are compared at the same one.
const glMajor = Number(flags.get("gl-major") ?? 0);
const glMinor = Number(flags.get("gl-minor") ?? 0);
if (backend._gex_gl_set_api)
  backend._gex_gl_set_api(api === "core" ? 1 : 0, glMajor, glMinor);

const init = backend._gex_gl_init(width, height);
if (init !== 0) {
  console.error(`FAIL gl-replay backend init returned ${init}`);
  process.exit(1);
}

function submit(bytes) {
  const input = backend._malloc(bytes.length || 1);
  if (bytes.length) backend.HEAPU8.set(bytes, input);
  const status = backend._gex_gl_submit(input, bytes.length);
  backend._free(input);
  return status;
}

function reply() {
  const length = backend._gex_gl_reply_len();
  const pointer = backend._gex_gl_reply_data();
  return backend.HEAPU8.slice(pointer, pointer + length);
}

/** Ask GL what it thinks of the batch that just ran. */
function glError() {
  const status = submit(command(op.GET_ERROR));
  if (status !== 0) return { status, error: 0 };
  const bytes = reply();
  if (bytes.length < 4) return { status: 0, error: 0 };
  return { status: 0, error: new DataView(bytes.buffer, bytes.byteOffset).getUint32(0, true) };
}

/**
 * The surface itself, in the container tools/surface-diff.py reads: the eight
 * bytes `GEXSURF\0`, width and height, then RGBA8 rows exactly as
 * glReadPixels returned them -- bottom-up, and the same on both backends, so
 * no orientation is imposed here.
 */
function writeSurface(path, out) {
  const view = new DataView(out.buffer, out.byteOffset, out.byteLength);
  const length = view.getUint32(4, true);
  const header = new Uint8Array(16);
  header.set(new TextEncoder().encode("GEXSURF\0"), 0);
  new DataView(header.buffer).setUint32(8, width, true);
  new DataView(header.buffer).setUint32(12, height, true);
  const blob = new Uint8Array(16 + length);
  blob.set(header, 0);
  blob.set(out.subarray(8, 8 + length), 16);
  writeFileSync(path, blob);
  if (!quiet) console.log(`SURFACE wrote ${path} ${width}x${height}`);
}

/**
 * A hash of the whole surface, so two backends replaying one trace can be
 * compared without shipping images around. FNV-1a: the value only has to be
 * stable and cheap, it is never a security property.
 */
function surfaceDigest() {
  const GL_RGBA = 0x1908;
  const GL_UNSIGNED_BYTE = 0x1401;
  // { x, y, w, h, format, type, pbo, pointer64 }; a zero PBO and pointer mean
  // "reply with the pixels" rather than "write them into guest memory".
  const payload = new Words()
    .i32(0).i32(0).i32(width).i32(height)
    .u32(GL_RGBA).u32(GL_UNSIGNED_BYTE).u32(0).u64(0n);

  const status = submit(command(op.READ_PIXELS, payload));
  if (status !== 0) return { status, digest: null, pixels: 0 };
  const out = reply();
  if (out.length < 8) return { status: 0, digest: null, pixels: 0 };
  if (dumpSurface) writeSurface(dumpSurface, out);
  const view = new DataView(out.buffer, out.byteOffset, out.byteLength);
  const length = view.getUint32(4, true);
  let hash = 0x811c9dc5;
  const sum = [0, 0, 0, 0];
  let covered = 0;
  for (let i = 0; i < length; i++) {
    hash ^= out[8 + i];
    hash = Math.imul(hash, 0x01000193) >>> 0;
    sum[i & 3] += out[8 + i];
  }
  // Coverage: pixels whose RGB is not the cleared background. An exact digest
  // cannot compare a CPU rasterizer with a GPU -- interpolation and
  // rasterization tie-breaks differ legitimately -- but the fraction of the
  // surface drawn to, and the mean colour, do compare.
  for (let i = 0; i + 3 < length; i += 4) {
    if (out[8 + i] || out[9 + i] || out[10 + i]) covered++;
  }
  const pixelCount = length >> 2;
  const mean = sum.map((v) => (pixelCount ? v / pixelCount : 0).toFixed(1));
  return {
    status: 0,
    digest: hash.toString(16).padStart(8, "0"),
    pixels: length,
    mean: mean.join(","),
    coverage: pixelCount ? ((100 * covered) / pixelCount).toFixed(1) : "0.0",
  };
}

let failed = null;
let replayed = 0;
for (const [index, batch] of trace.batches.entries()) {
  if (index >= stopAfter) break;
  const status = submit(batch.bytes);
  replayed++;
  if (status !== 0) {
    failed = { index, reason: `decoder status ${status}` };
    break;
  }
  if (allowGlErrors) continue;
  const { status: probeStatus, error } = glError();
  if (probeStatus !== 0) {
    failed = { index, reason: `GET_ERROR probe rejected with status ${probeStatus}` };
    break;
  }
  if (error !== 0) {
    failed = { index, reason: `GL error 0x${error.toString(16)}` };
    break;
  }
}

if (failed) {
  console.error(
    `FAIL gl-replay batch=${failed.index}/${trace.batches.length} ${failed.reason}`,
  );
  process.exit(1);
}

if (wantReadback) {
  const { status, digest, pixels, mean, coverage } = surfaceDigest();
  if (status !== 0 || digest === null) {
    console.error(`FAIL gl-replay readback rejected with status ${status}`);
    process.exit(1);
  }
  console.log(
    `REPLAY digest=${digest} pixels=${pixels} size=${width}x${height} ` +
      `mean=${mean} coverage=${coverage}%`,
  );
}

if (!quiet)
  console.log(
    `PASS gl-replay batches=${replayed} protocol=${trace.info.protocolVersion} size=${width}x${height}`,
  );
