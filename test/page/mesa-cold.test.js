// Mesa's cold part: does the split replay module draw what the whole one
// does, and does a miss load the cold part and carry on? For each of the mt
// and st builds (the `mesa-cold` image), fixtures/gl's gles-gears trace is
// replayed on WebGPU, the last frame read back (READ_PIXELS, which waits on a
// buffer map: a JSPI suspension) and hashed, through:
//   whole     the module unsplit (the reference)
//   split     the build's own split, its cold part loaded with the module (as
//             a page's main thread does)
//   miss      every function kept but glReadPixels', left for a miss: the
//             cold part is fetched and compiled synchronously, and the
//             read-back then waits on WebGPU inside a function it brought
//   prefetch  the same once the background prefetch (after the first
//             frame) has compiled it: instantiated only
// Every digest must be the whole module's, and each miss taken as described.
// One leg a page (t.reload); without SharedArrayBuffer (the single-thread
// pass) only the st build's.

export const meta = { hosts: ['browser'], arches: ['x86'], images: ['mesa-cold'], timeout: 1200 };

const LEGS = [['whole', 'whole', {}], ['split', 'split', {}], ['miss', 'miss', { lazy: true, noprefetch: true }], ['prefetch', 'miss', { lazy: true, prefetch: true }]];

/** A canvas whose WebGPU context draws into a texture the replay can read
 *  back (an OffscreenCanvas's own cannot be copied from). */
function textureCanvas() {
  const canvas = new OffscreenCanvas(16, 16);
  let config = null;
  let texture = null;
  const context = {
    canvas,
    configure(c) { config = c; },
    unconfigure() { config = null; },
    getConfiguration() { return config; },
    getCurrentTexture() {
      if (!texture || texture.width !== canvas.width || texture.height !== canvas.height) {
        texture?.destroy();
        texture = config.device.createTexture({
          size: [canvas.width, canvas.height], format: config.format, viewFormats: config.viewFormats ?? [],
          usage: (config.usage ?? GPUTextureUsage.RENDER_ATTACHMENT) | GPUTextureUsage.TEXTURE_BINDING | GPUTextureUsage.COPY_DST | GPUTextureUsage.COPY_SRC,
        });
      }
      return texture;
    },
  };
  const own = canvas.getContext.bind(canvas);
  canvas.getContext = (type, o) => (type === 'webgpu' ? context : own(type, o));
  return canvas;
}

/** Whether a batch presents (a frame's end). */
function hasPresent(b) {
  const v = new DataView(b.buffer, b.byteOffset, b.byteLength);
  for (let at = 0; at + 8 <= b.length;) {
    const op = v.getUint32(at, true);
    const size = v.getUint32(at + 4, true);
    if (op === 4) return true;
    if (size < 8) break;
    at += size;
  }
  return false;
}

/** The trace through `mesa` (its glue's URL): `{ digest, frames, rejects,
 *  validation, cold }`. */
async function replay(t, mesa, trail, { lazy = false, noprefetch = false, settle = async () => {} }) {
  const adapter = await navigator.gpu.requestAdapter();
  if (!adapter) throw new Error('no WebGPU adapter');
  const optional = ['texture-compression-bc', 'depth32float-stencil8', 'rg11b10ufloat-renderable', 'depth-clip-control', 'float32-filterable', 'float32-blendable', 'clip-distances', 'dual-source-blending'];
  const device = await adapter.requestDevice({ requiredFeatures: optional.filter((f) => adapter.features.has(f)) });
  let validation = 0;
  device.addEventListener('uncapturederror', (e) => { if (validation++ < 5) t.say(`  WebGPU validation: ${e.error?.message}`); });
  const glCanvas = textureCanvas();
  const qs = document.querySelector.bind(document);
  document.querySelector = (s) => (String(s).includes('gex-gl') ? glCanvas : qs(s));
  const { default: create } = await import(/* @vite-ignore */ mesa);
  const m = await create({
    preinitializedWebGPUDevice: device, mainScriptUrlOrBlob: mesa, locateFile: (f) => new URL(f, mesa).href,
    gexColdLazy: lazy, gexNoColdPrefetch: noprefetch, print: () => {}, printErr: () => {},
  });
  const submit = WebAssembly.promising(m.gexGlSubmitRaw);
  const bytes = new Uint8Array(await (await fetch(t.work('mesa-cold/gears.gexgltr'))).arrayBuffer());
  const dv = new DataView(bytes.buffer);
  const W = dv.getUint32(16, true);
  const H = dv.getUint32(20, true);
  glCanvas.width = W;
  glCanvas.height = H;
  if (m._gex_gl_init(W, H) !== 0) throw new Error('gex_gl_init failed');
  let cap = 1 << 20;
  let ptr = m._malloc(cap);
  let frames = 0;
  let rejects = 0;
  for (let at = 24; at < bytes.length;) {
    const len = dv.getUint32(at, true);
    const b = bytes.subarray(at + 8, at + 8 + len);
    at += 8 + len + ((4 - (len & 3)) & 3);
    if (b.length > cap) { m._free(ptr); cap = b.length; ptr = m._malloc(cap); }
    m.HEAPU8.set(b, ptr);
    if ((await submit(ptr, b.length, 0)) !== 0) rejects++;
    if (hasPresent(b)) {
      m._gex_gl_display();
      trail.frames = ++frames;
      if (frames % 32 === 0) await device.queue.onSubmittedWorkDone();
    }
  }
  await device.queue.onSubmittedWorkDone();
  await settle().catch((e) => {
    throw new Error(`${e.message} (the cold part ${m.gexColdLoaded ? `loaded ${m.gexColdLoaded}` : 'not loaded'}; ${trail.seen.join(' | ') || 'nothing logged'})`);
  });
  // READ_PIXELS of the default framebuffer, RGBA/UNSIGNED_BYTE.
  const cmd = new Uint8Array(44);
  const cv = new DataView(cmd.buffer);
  [128, cmd.length, 0, 0, W, H, 0x1908, 0x1401, 0, 0, 0].forEach((w, i) => cv.setUint32(i * 4, w, true));
  m.HEAPU8.set(cmd, ptr);
  if ((await submit(ptr, cmd.length, 0)) !== 0) rejects++;
  const rlen = m._gex_gl_reply_len();
  const rat = m._gex_gl_reply_data();
  const px = m.HEAPU8.slice(rat + 8, rat + rlen);
  let h = 0x811c9dc5;
  for (let i = 0; i < px.length; i++) { h ^= px[i]; h = Math.imul(h, 0x01000193) >>> 0; }
  // How the cold part came in (the glue's `gexColdLoaded`): 'with the
  // module', 'on a miss', or not at all.
  return { digest: `${h.toString(16)}/${px.length}`, frames, rejects, validation, cold: m.gexColdLoaded ?? null };
}

export default async function (t) {
  const builds = typeof SharedArrayBuffer === 'function' ? ['mt', 'st'] : ['st'];
  const all = builds.flatMap((b) => LEGS.map((l) => [b, ...l]));
  const state = t.state ?? { refs: {} };
  const [build, name, dir, opts] = all[t.phase - 1];
  const { cold: hasCold } = await (await fetch(t.work(`mesa-cold/${build}/split.json`))).json();
  // The glue's own lines about the cold part, as it logs them.
  const seen = [];
  // Each line with the frame the replay was at.
  const trail = { frames: 0, seen };
  for (const k of ['log', 'info', 'warn']) {
    const own = console[k].bind(console);
    console[k] = (...a) => { const s = a.map(String).join(' '); if (s.includes('cold part')) seen.push(`${s} (frame ${trail.frames})`); own(...a); };
  }
  let r;
  try {
    const settle = () => t.until(() => seen.some((l) => l.includes('cold part prefetched')), `${build} ${name}: the background prefetch`, 60000);
    r = await t.timeout(replay(t, t.work(`mesa-cold/${build}/${dir}/gex-wgpu-replay.js`), trail, { ...opts, ...(opts.prefetch ? { settle } : {}) }), 300000, `${build} ${name}`);
  } catch (e) {
    t.check(`${build} ${name}: replays`, false, String(e?.stack || e));
  }
  if (r) {
    if (r.cold) seen.unshift(`cold part loaded ${r.cold}`);
    const how = seen.join(' | ') || 'no cold part loaded';
    t.check(`${build} ${name}: no rejected batch or validation error`, r.rejects === 0 && r.validation === 0, `${r.rejects} rejected, ${r.validation} validation errors`);
    state.refs[build] ??= r.digest;
    t.eq(`${build} ${name}: the whole module's digest`, r.digest, state.refs[build]);
    const want = { whole: [], split: hasCold ? ['loaded with the module'] : [], miss: ['loaded on a miss', 'loaded on a miss (fetched and compiled now'], prefetch: ['loaded on a miss', 'loaded on a miss (prefetched)'] }[name];
    t.check(`${build} ${name}: the cold part as wanted (${want.join(', ') || 'none'})`, want.every((w) => seen.some((l) => l.includes(w))) && (want.length > 0 || seen.length === 0), how);
    t.say(`  ${build} ${name}: digest ${r.digest}, ${r.frames} frames; ${how}`);
  }
  if (t.phase < all.length) return t.reload(state);
}
