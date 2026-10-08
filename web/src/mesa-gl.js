import { requestGexDevice } from './gpu-device.js';
// The guest's GL device, backed by Mesa's frontend and the gex_wgpu Gallium
// driver on the browser's WebGPU.
//
// It is handed the
// guest's command ring and executes it. What differs is everything below the
// decoder -- here the ring goes to gex's wire decoder compiled against
// st/mesa, so the GL semantics are Mesa's rather than hand-written.
//
// `tests/mesa-wgpu-replay.sh` exercises the same module against a recorded
// trace and compares it with softpipe; this is the live path, for running real
// guest programs and the CTS.

import { MAGIC, PROTOCOL_VERSION, HEADER_BYTES, error } from './gl-proto.js';
import { captureIfRequested } from './gl-capture.js';

// Header words, matching crates/gex-gl/src/proto.rs.
const H_MAGIC = 0;
const H_CAPACITY = 1;
const H_TAIL = 2;
const H_REPLY_CAP = 3;
const H_REPLY_LEN = 4;
const H_STATUS = 5;
const H_FAILED_AT = 6;
const H_PROTOCOL_VERSION = 7;

// Imported directly where that works: Emscripten resolves its .wasm against the
// module's own URL, so a real URL keeps `locateFile` and the pthread workers on
// the same simple footing.
//
// It does not work under `vite dev`. The glue lives in web/public, and vite
// refuses to transform anything there -- "This file is in /public and will be
// copied as-is during build without going through the plugin transforms" -- so
// the request the module loader makes (with vite's `?import` appended) is a
// 500. The built site has no such middleware and serves the file as-is, which
// is why this only ever failed on the dev server.
//
// So: try the real URL, and fall back to a blob. `mainScriptUrlOrBlob` gives
// Emscripten back the true location, which is what its worker bootstrap needs
// once `import.meta.url` is a blob: URL.
async function loadGlue(url) {
  // Under `vite dev` the direct import cannot work, so do not attempt it: the
  // request 500s, vite reports it, and the noise reads as a crash even though
  // the fallback below succeeds. `import.meta.env.DEV` is false in the build,
  // where the direct import is the right one.
  if (!import.meta.env?.DEV) {
    try {
      return (await import(/* @vite-ignore */ url)).default;
    } catch {
      /* fall through to the blob */
    }
  }
  const res = await fetch(url, { cache: 'no-store' });
  if (!res.ok) throw new Error(`${url}: ${res.status} ${res.statusText}`);
  const blob = URL.createObjectURL(
    new Blob([await res.text()], { type: 'text/javascript' }));
  try {
    return (await import(/* @vite-ignore */ blob)).default;
  } finally {
    URL.revokeObjectURL(blob);
  }
}

/** The driver's WebGPU calls by name: [calls, ms inside, bytes, ms waited]
 *  ("waited" is a promise's time to settle: mapAsync, onSubmittedWorkDone).
 *  `glState` reports them per frame since it was last asked. Only when asked
 *  for (`?wgpustats=1`, the `wgpuStats` option): the wrapper is a
 *  performance.now pair and a Map lookup on each of a few thousand calls a
 *  frame, which was 2.5% of the GL worker's time in Noita's page. */
const wgpuCalls = new Map();
function countWebGpu() {
  if (countWebGpu.done || typeof GPUQueue === 'undefined') return;
  countWebGpu.done = true;
  const entry = (name) => {
    let e = wgpuCalls.get(name);
    if (!e) wgpuCalls.set(name, (e = [0, 0, 0, 0]));
    return e;
  };
  const wrap = (proto, name, bytesOf) => {
    const f = proto?.[name];
    if (typeof f !== 'function') return;
    proto[name] = function (...a) {
      const t = performance.now();
      const r = f.apply(this, a);
      const e = entry(name);
      e[0]++;
      e[1] += performance.now() - t;
      if (bytesOf) {
        try { e[2] += bytesOf(a) || 0; } catch {}
      }
      if (r && typeof r.then === 'function') {
        r.then(() => { e[3] += performance.now() - t; }, () => {});
      }
      return r;
    };
  };
  const view = (d, off, size) => {
    const el = d?.BYTES_PER_ELEMENT ?? 1;
    return size !== undefined ? size * el : (d?.byteLength ?? 0) - (off ?? 0) * el;
  };
  wrap(GPUQueue.prototype, 'writeBuffer', (a) => view(a[2], a[3], a[4]));
  wrap(GPUQueue.prototype, 'writeTexture', (a) => a[1]?.byteLength ?? 0);
  wrap(GPUQueue.prototype, 'submit');
  wrap(GPUQueue.prototype, 'onSubmittedWorkDone');
  wrap(GPUCommandEncoder.prototype, 'finish');
  wrap(GPUCommandEncoder.prototype, 'beginRenderPass');
  wrap(GPUCommandEncoder.prototype, 'copyBufferToBuffer');
  wrap(GPUCommandEncoder.prototype, 'copyBufferToTexture');
  wrap(GPUCommandEncoder.prototype, 'copyTextureToTexture');
  wrap(GPUDevice.prototype, 'createCommandEncoder');
  wrap(GPUDevice.prototype, 'createBindGroup');
  wrap(GPUDevice.prototype, 'createBuffer', (a) => a[0]?.size ?? 0);
  wrap(GPUDevice.prototype, 'createTexture');
  wrap(GPUDevice.prototype, 'createRenderPipeline');
  wrap(GPUBuffer.prototype, 'mapAsync');
  wrap(GPURenderPassEncoder.prototype, 'draw');
  wrap(GPURenderPassEncoder.prototype, 'drawIndexed');
  wrap(GPURenderPassEncoder.prototype, 'setBindGroup');
  wrap(GPURenderPassEncoder.prototype, 'setPipeline');
}

/** Mesa's periodic reports ("gex-wgpu: ..." lines its present path prints
 *  every thirty frames, nine at a time) at most once a minute each, keyed by
 *  their first words; everything else as it comes. Each console line is a
 *  message to whatever is listening (DevTools, a harness over CDP), on the
 *  thread that runs Mesa: two thousand batches in a seven-minute Noita run.
 *  `?gltrace=1` keeps them all. */
/** A page's `?NAME=1`. In a Worker `location` is the worker script's, so
 *  the compositor passes its page's switches in as options instead. */
function pageFlag(name) {
  try {
    return globalThis.document !== undefined && new URLSearchParams(location.search).get(name) === '1';
  } catch {
    return false;
  }
}

function quietReports(out) {
  let all = false;
  try {
    all = new URLSearchParams(location.search).get('gltrace') === '1';
  } catch {}
  if (all) return out;
  const last = new Map();
  return (line) => {
    const m = /^gex-wgpu: ([a-z -]{4,40}?)(?: --|:| [0-9])/.exec(String(line));
    if (m) {
      const now = performance.now();
      const at = last.get(m[1]);
      if (at !== undefined && now - at < 60000) return;
      last.set(m[1], now);
    }
    out(line);
  };
}

/** One IndexedDB store of shader-cache blobs, by Mesa glue URL. */
function wgslDb(op, key, value) {
  return new Promise((resolve, reject) => {
    const open = indexedDB.open('gex-wgsl-cache', 1);
    open.onupgradeneeded = () => open.result.createObjectStore('blobs');
    open.onerror = () => reject(open.error);
    open.onsuccess = () => {
      const db = open.result;
      const tx = db.transaction('blobs', op === 'get' ? 'readonly' : 'readwrite');
      const req = op === 'get' ? tx.objectStore('blobs').get(key) : tx.objectStore('blobs').put(value, key);
      req.onsuccess = () => resolve(req.result);
      req.onerror = () => reject(req.error);
      tx.oncomplete = () => db.close();
    };
  });
}

export class MesaGlDevice {
  // The GPUAdapter and GPUDevice, held for the renderer's whole life and never
  // read again.
  //
  // Nothing else keeps a JavaScript reference to either. The device is handed
  // to Emscripten as `preinitializedWebGPUDevice` and from then on only the
  // wasm side has a handle, which is not a reference the browser's collector
  // can see; the adapter was a local that went out of scope as soon as
  // `create` returned. Chromium then collected them -- `device.lost` resolved
  // with reason "destroyed" without anything ever calling `destroy()` -- about
  // a second into SuperTuxKart's first frame.
  //
  // The adapter matters as much as the device: it owns the Dawn instance
  // behind both, and the readback that finally surfaced this failed with
  // Dawn's own words, "A valid external Instance reference no longer exists".
  // Until then nothing complained at all -- a destroyed device does not fail
  // draws or raise validation errors, it just stops doing anything, so the
  // frame counters climbed and hundreds of thousands of draws were "issued"
  // into a device that no longer existed. Small demos never showed it because
  // they finish before a collection runs.
  #adapter = null;
  #device = null;
  #module = null;
  #submit = null;
  #failed = null;
  /** Batches this device refused, and the last of them: a client's own
   *  problem, kept where a page can see it. */
  #rejects = 0;
  #lastReject = '';
  #description = '';
  #submits = 0;
  #bytes = 0;
  // Wall time actually spent inside a batch, so "the Mesa path is slow" can be
  // attributed rather than assumed.
  #submitMs = 0;
  #readbacks = 0;
  #readbackMs = 0;
  #maxSubmitMs = 0;
  #busy = false;
  #lastPresented = 0;
  #lastProduced = 0;
  static #reported = false;

  /**
   * `api` is 'es' or 'core'; `major`/`minor` ask st/mesa for a version, with
   * zero meaning the backend's default. The driver's caps decide what it can
   * actually give, so asking for more than they support fails context
   * creation rather than silently downgrading.
   */
  static async create(canvas, options = {}) {
    const {
      api = 'es',
      major = 0,
      minor = 0,
      url = '/gex/gex-wgpu-replay.js',
    } = options;
    const out = new MesaGlDevice();
    canvas.id = 'gex-gl';
    try {
      // The page's shared device (gpu-device.js): the compositor samples the
      // frames this one draws, so it must be the same device.
      const { adapter, device } = options.gpu ?? await requestGexDevice();
      out.#adapter = adapter;
      out.#device = device;
      if (options.wgpuStats ?? pageFlag('wgpustats')) countWebGpu();
      // And out of reach of the collector for as long as the page lives.
      //
      // The fields above were supposed to be enough, and are not: the device
      // still resolved its `lost` promise with "destroyed" a few frames in,
      // with nothing calling `destroy()` -- a wrapper on the method proved
      // that -- and a destroyed device fails nothing, it simply stops, which
      // is a black window and climbing frame counters. The queue is held too,
      // since the device's collection took it with them.
      //
      // Not tidy. A GL device is created once or twice in a page's life, and
      // an object that outlives its owner is a cheaper mistake than a device
      // collected in the middle of a frame.
      // Who destroyed it, if anyone does. `wgpuDeviceDestroy` from the wasm
      // side is this method in emdawnwebgpu, so a wrapper here names the
      // caller -- and a device that is destroyed stops doing anything without
      // failing a single call, which is invisible from every other angle.
      {
        const destroy = device.destroy.bind(device);
        device.destroy = () => {
          console.error('[mesa-gl] the WebGPU device is being destroyed',
                        new Error('destroy').stack);
          destroy();
        };
      }
      // A lost device explains everything downstream at once -- every map
      // aborts, every submit is dropped -- and says nothing about itself
      // through the normal error channel, so it has to be watched for
      // separately. `mapAsync` rejecting with "A valid external Instance
      // reference no longer exists" is what a loss looks like from the driver.
      device.lost.then((info) => {
        const message = `${info.reason}: ${info.message}`;
        console.error('[mesa-gl] WebGPU device lost:', message);
        if (!out.#failed) out.#failed = `WebGPU device lost: ${message}`;
      });
      device.addEventListener('uncapturederror', (event) => {
        const message = event.error?.message || String(event.error);
        console.error('[mesa-gl] WebGPU validation:', message);
        // A validation error poisons the WebGPU command buffer, so what the
        // guest sees afterwards is not GL's answer. Record the first one.
        if (!out.#failed) out.#failed = `WebGPU validation: ${message}`;
      });

      const Module = await loadGlue(url);
      out.#module = await Module({
        preinitializedWebGPUDevice: device,
        // Where the glue really lives, for the blob fallback above: Emscripten
        // starts its pthread workers from this rather than `import.meta.url`.
        mainScriptUrlOrBlob: new URL(url, location.href).href,
        locateFile: (file) => new URL(file, new URL(url, location.href)).href,
        print: (line) => console.log('[mesa-gl]', line),
        printErr: quietReports((line) => console.warn('[mesa-gl]', line)),
      });

      // Readback suspends the wasm stack, so submission goes through JSPI.
      out.#submit = WebAssembly.promising(out.#module.gexGlSubmitRaw);
      if (out.#module._gex_gl_set_api)
        out.#module._gex_gl_set_api(api === 'core' ? 1 : 0, major, minor);
      // Environment for Mesa (`?mesaenv=`), before the context exists.
      for (const pair of (options.env || '').split(',').filter(Boolean)) {
        if (!out.#module._gex_gl_setenv) break;
        const bytes = new TextEncoder().encode(`${pair}\0`);
        const at = out.#module._malloc(bytes.length);
        out.#module.HEAPU8.set(bytes, at);
        out.#module._gex_gl_setenv(at);
        out.#module._free(at);
      }
      // Shader translations from before (Tint's SPIR-V -> WGSL, most of a
      // shader compile): the site's recorded set and this browser's own.
      await out.#loadWgslCache(url, options.wgslCache ?? 'all');
      const rc = out.#module._gex_gl_init(canvas.width, canvas.height);
      if (rc !== 0) out.#failed = `Mesa/WebGPU initialization failed (${rc})`;
      out.#description = `Mesa st/mesa + gex_wgpu on WebGPU (${api})`;
      out.capture = captureIfRequested(PROTOCOL_VERSION, canvas.width, canvas.height, options.capture);
    } catch (e) {
      out.#failed = e?.message || String(e);
    }
    return out;
  }

  // -- the shader translation cache (gex_wgpu_tint.cpp) --------------------
  //
  // Blobs of SPIR-V -> WGSL answers, each refused by Mesa unless this very
  // build made it. Two sources, both before the first compile: the site's
  // recorded set (`gex-wgsl-cache.bin` beside the Mesa glue, packed by
  // eject-site from a training run) and the browser's IndexedDB, to which
  // new translations are saved every few seconds while they keep coming.
  #wgslKey = null;
  #wgslSaved = 0;

  /** `mode`: 'all' (the site's and this browser's), 'site' (the site's
   *  only, nothing kept: bench runs, whose profile outlives a run), '0'. */
  async #loadWgslCache(url, mode) {
    const m = this.#module;
    if (!m._gex_wgsl_cache_load || mode === '0') return;
    const glue = new URL(url, location.href);
    this.#wgslKey = glue.href;
    const load = (bytes, from) => {
      if (!bytes?.byteLength) return;
      const at = m._malloc(bytes.byteLength);
      m.HEAPU8.set(new Uint8Array(bytes), at);
      const n = m._gex_wgsl_cache_load(at, bytes.byteLength);
      m._free(at);
      console.log(`[mesa-gl] shader cache: ${n < 0 ? 'refused (another build)' : `${n} translations`} from ${from}`);
    };
    try {
      const r = await fetch(new URL('gex-wgsl-cache.bin', glue));
      if (r.ok) load(await r.arrayBuffer(), 'the site');
    } catch { /* none shipped */ }
    if (mode === 'site') {
      this.#wgslKey = null;
      return;
    }
    try {
      load(await wgslDb('get', this.#wgslKey), 'this browser');
    } catch { /* no IndexedDB here */ }
    const timer = setInterval(() => this.#saveWgslCache(), 5000);
    (globalThis.__gexWgslTimers ||= []).push(timer);
  }

  /** The whole cache as one blob (an ArrayBuffer), or null. */
  wgslCache(onlyFresh = false) {
    const m = this.#module;
    if (!m?._gex_wgsl_cache_dump) return null;
    const lenAt = m._malloc(4);
    const at = m._gex_wgsl_cache_dump(onlyFresh ? 1 : 0, lenAt);
    const len = new Uint32Array(m.HEAPU8.buffer, lenAt, 1)[0];
    m._free(lenAt);
    if (!at) return null;
    const blob = m.HEAPU8.slice(at, at + len).buffer;
    m._free(at);
    return blob;
  }

  /** hits, misses, loaded, held. */
  wgslStats() {
    const m = this.#module;
    if (!m?._gex_wgsl_cache_stats) return null;
    const at = m._malloc(16);
    m._gex_wgsl_cache_stats(at);
    const [hits, misses, loaded, held] = new Uint32Array(m.HEAPU8.buffer, at, 4);
    m._free(at);
    return { hits, misses, loaded, held };
  }

  async #saveWgslCache() {
    const st = this.wgslStats();
    if (!this.#wgslKey || !st || st.misses === this.#wgslSaved) return;
    this.#wgslSaved = st.misses;
    try {
      await wgslDb('put', this.#wgslKey, this.wgslCache());
    } catch { /* best effort */ }
  }

  get ok() {
    return this.#module !== null && this.#failed === null;
  }

  get error() {
    return this.#failed;
  }

  describe() {
    return this.#failed ? `unavailable: ${this.#failed}` : this.#description;
  }

  stats() {
    return `${this.#submits} submits, ${(this.#bytes / 1024) | 0} KiB`;
  }

  /** Run one batch. `memory` holds the ring; `ring` is its offset.
   *
   * `client` names the guest process the batch came from: it gets a GL
   * context of its own on the host, because the object names in the stream
   * are its own and another process hands out the same ones. */
  submit(memory, ring, client = 0) {
    if (!this.#module) return -1;
    const words = new Uint32Array(memory.buffer, ring, HEADER_BYTES / 4);
    if (words[H_MAGIC] !== MAGIC) return -1;
    if (words[H_PROTOCOL_VERSION] !== PROTOCOL_VERSION) {
      words[H_STATUS] = error.VERSION_MISMATCH;
      return -1;
    }
    const capacity = words[H_CAPACITY];
    const tail = words[H_TAIL];
    const replyCap = words[H_REPLY_CAP];
    if (tail > capacity) return -1;

    const started = performance.now();
    this.#submits++;
    this.#bytes += tail;
    const src = new Uint8Array(memory.buffer, ring + HEADER_BYTES, tail);
    if (this.capture && !this.capture.record(src, replyCap) && !this.capture.posted) {
      // Full: hand the recording to whoever serves the page (a Worker has
      // no CDP-reachable global the page's driver can read).
      this.capture.posted = true;
      const bytes = this.capture.toBytes();
      this.capture = null;
      fetch('/__gex_capture', { method: 'POST', body: bytes }).catch(() => {});
    }

    const ptr = this.#module._malloc(tail || 1);
    if (tail) this.#module.HEAPU8.set(src, ptr);

    const finish = (rc) => {
      const elapsed = performance.now() - started;
      this.#submitMs += elapsed;
      // A frame's readback (the NT host reads every frame back to blit it
      // into the window): what the GPU -- SwiftShader, headless -- took.
      if ((this.#module?._gex_gl_reply_len?.() ?? 0) >= 256 * 1024) {
        this.#readbacks++;
        this.#readbackMs += elapsed;
      }
      if (elapsed > this.#maxSubmitMs) this.#maxSubmitMs = elapsed;
      this.#module._free(ptr);
      if (rc !== 0) {
        // Terminal for that *client*: a command may have executed a valid
        // prefix before its malformed tail was found, so neither the reply
        // nor that client's GL context is safe to reuse -- and the guest
        // latches on the failed ioctl by itself.
        //
        // Not terminal for the device, which is the difference several
        // clients make. Marking the device failed here stopped every other
        // client's OpenGL as well: one program with one bad batch took the
        // compositor's whole GL path with it, and the client that lost its
        // window was usually not the one at fault.
        this.#rejects++;
        const m = this.#module;
        const why = ['', 'short read', 'trailing data', 'unknown opcode',
                     'no context'][m._gex_gl_reject_why?.() ?? 0] || 'refused';
        this.#lastReject = `client ${client} batch rejected (${rc}): opcode `
          + `${m._gex_gl_reject_op?.() ?? '?'} ${why}, payload `
          + `${m._gex_gl_reject_payload?.() ?? '?'}`;
        console.error(`[mesa-gl] ${this.#lastReject}`);
        words[H_STATUS] = error.BAD_OPCODE;
        words[H_REPLY_LEN] = 0;
        words[H_TAIL] = 0;
        words[H_FAILED_AT] = 0;
        return rc;
      }
      const replyLen = this.#module._gex_gl_reply_len();
      if (replyLen > replyCap) {
        words[H_STATUS] = error.BAD_ARGUMENT;
        words[H_REPLY_LEN] = 0;
        words[H_TAIL] = 0;
        return -1;
      }
      if (replyLen) {
        const from = this.#module._gex_gl_reply_data();
        new Uint8Array(memory.buffer, ring + HEADER_BYTES + capacity, replyLen)
          .set(this.#module.HEAPU8.subarray(from, from + replyLen));
      }
      words[H_TAIL] = 0;
      words[H_REPLY_LEN] = replyLen;
      words[H_STATUS] = error.NONE;
      words[H_FAILED_AT] = 0;
      return 0;
    };

    this.#busy = true;
    const done = (rc) => { this.#busy = false; return finish(rc); };
    // The frame names only, on one line. A thrown stack is dozens of long
    // URLs; the harness captures a reflowing terminal and mangles them, which
    // is how the function under util_blitter_blit stayed unnamed. Names alone
    // fit, and they are the part that identifies the fault.
    const report = (e) => {
      this.#busy = false;
      if (!MesaGlDevice.#reported) {
        MesaGlDevice.#reported = true;
        // Innermost frames first, one short line each: long lines are
        // reflowed by the harness terminal and merge into each other.
        const lines = String(e?.stack || '').split('\n').slice(0, 8);
        lines.forEach((line, i) => {
          const m = line.match(/wasm\.([A-Za-z_][A-Za-z0-9_]*)/);
          console.error('[mesa-gl] frame' + i + ' ' +
                        (m ? m[1] : line.trim().slice(0, 52)));
        });
      }
      throw e;
    };
    let result;
    try {
      result = this.#submit(ptr, tail, client >>> 0);
    } catch (e) {
      return report(e);
    }
    return result instanceof Promise
      ? result.then(done, report)
      : done(result);
  }

  /** What the guest says its frames belong to: see `op::SURFACE_ATTACH`.
   *
   * `kind` 0 is a Wayland surface named with the process that published it,
   * 1 an X drawable. A zero `id` means the guest never said, which is the
   * case for anything drawing to the output rather than into a window. */
  attachment() {
    const m = this.#module;
    if (!m?._gex_gl_attached_id) return null;
    return {
      kind: m._gex_gl_attached_kind(),
      id: m._gex_gl_attached_id(),
      token: m._gex_gl_attached_token(),
      generation: m._gex_gl_attached_generation(),
    };
  }

  /** Every window the host is drawing, one entry each.
   *
   * `frame` is what the renderer keys that window's picture by -- pass it to
   * `renderer.frameFor` -- and `kind`/`id`/`token` are the window itself, as
   * `attachment()` describes them. The list is what makes more than one GL
   * window possible: a compositor asks per window rather than being told
   * about whichever one presented last. */
  surfaces() {
    const m = this.#module;
    if (!m?._gex_gl_surface_count) {
      // A module without the enumeration: report its single attachment in the
      // same shape, so callers need no second path.
      const one = this.attachment();
      return one && one.id ? [{ ...one, frame: 0, client: one.token }] : [];
    }
    const out = [];
    const count = m._gex_gl_surface_count();
    for (let i = 0; i < count; i++) {
      out.push({
        kind: m._gex_gl_surface_kind(i),
        id: m._gex_gl_surface_id(i),
        token: m._gex_gl_surface_token(i),
        client: m._gex_gl_surface_client(i),
        frame: m._gex_gl_surface_frame(i),
        width: m._gex_gl_surface_width(i),
        height: m._gex_gl_surface_height(i),
      });
    }
    return out;
  }

  /** Bumped whenever a window is attached or goes away. */
  surfaceGeneration() {
    return this.#module?._gex_gl_surface_generation?.() ?? 0;
  }

  /** That process is gone: release its context, its windows and their frames.
   *
   * A client that exits says nothing -- it is not there to say it -- so the
   * page says it instead. Its frames go too, which is what stops a
   * compositor drawing the last picture of a window that has closed. */
  forgetClient(client) {
    const m = this.#module;
    if (!m?._gex_gl_forget_client || !client) return;
    m._gex_gl_forget_client(client >>> 0);
  }

  /** Present whatever the guest last drew. */
  display() {
    // Not while a submit is suspended. Readback makes submit a JSPI call that
    // parks mid-flight, and the page's animation loop calls display() on every
    // frame regardless -- re-entering the wasm module on the same thread while
    // it holds a lock. musl catches that ("pthread mutex deadlock detected"),
    // the throw unwinds to the guest as a failed ioctl, and the queued batch
    // is discarded: GL objects created in it are lost, and the damage shows up
    // much later somewhere unrelated. A frame skipped here costs one present.
    if (this.#busy) return 0;
    return this.#module?._gex_gl_display?.() ?? 0;
  }

  // The rest of the renderer interface a directGl demo uses. This device
  // decodes on the page's own event loop, so a present is simply a call that
  // has returned and there is no worker to talk to. Answering honestly here
  // is what lets a directGl demo -- a real application, not a demo written
  // for this driver -- run on it unchanged.
  // The backend's own counters, not a tally of display() calls. `produced`
  // advances when the guest swaps and `presented` when a swapped frame is
  // taken, so the two disagreeing is meaningful -- counting calls here would
  // report a frame rate for a backend that drew nothing, which is exactly the
  // reading a present counter exists to prevent.
  // Cached whenever a submit is suspended. These are read from a page
  // animation frame -- the harness polls __gexGlState() every frame -- and
  // calling into the module then re-enters it on the same thread while JSPI
  // has it parked mid-submit, which musl reports as a mutex deadlock and
  // which costs the whole queued batch. The last known value is right often
  // enough for a counter that only moves on present.
  presentedCount() {
    if (!this.#busy) this.#lastPresented = this.#module?._gex_gl_presented?.() ?? 0;
    return this.#lastPresented;
  }

  producedCount() {
    if (!this.#busy) this.#lastProduced = this.#module?._gex_gl_produced?.() ?? 0;
    return this.#lastProduced;
  }

  /** No worker: the caller serves the ring on the page's loop instead. */
  attachChannel() {
    return null;
  }

  refresh() {
    return Promise.resolve();
  }

  traceCommands() {}

  // Read by the test harness (via __gexGlState) to tell whether the client is
  // rendering, which it does by matching `presented=<n>`. Returning null here
  // made the harness read every run as zero frames regardless of what the
  // backend did.
  glState() {
    if (!this.ok) return this.#failed ?? '';
    return (this.#rejects ? `rejects=${this.#rejects} (${this.#lastReject}) ` : '') +
      `presented=${this.presentedCount()} produced=${this.producedCount()} ` +
      `mesa=1 submits=${this.#submits} bytes=${this.#bytes} ` +
      `submit_ms=${this.#submitMs.toFixed(1)} ` +
      `avg_ms=${(this.#submitMs / Math.max(1, this.#submits)).toFixed(3)} ` +
      `max_ms=${this.#maxSubmitMs.toFixed(1)} ` +
      `readbacks=${this.#readbacks} readback_ms=${this.#readbackMs.toFixed(1)}` +
      this.#wgpuSince();
  }

  #wgpuLast = { frames: 0, calls: new Map() };
  /** The WebGPU calls per frame since the last report: name calls/ms[/KiB][/waited ms]. */
  #wgpuSince() {
    if (!wgpuCalls.size) return '';
    const frames = this.producedCount();
    const f = Math.max(1, frames - this.#wgpuLast.frames);
    const parts = [];
    for (const [name, e] of [...wgpuCalls].sort((a, b) => b[1][1] - a[1][1])) {
      const p = this.#wgpuLast.calls.get(name) ?? [0, 0, 0, 0];
      const d = e.map((v, i) => v - p[i]);
      if (!d[0]) continue;
      let s = `${name} ${(d[0] / f).toFixed(1)}/${(d[1] / f).toFixed(2)}ms`;
      if (d[2]) s += `/${(d[2] / f / 1024).toFixed(0)}KiB`;
      if (d[3]) s += `/wait ${(d[3] / f).toFixed(2)}ms`;
      parts.push(s);
    }
    this.#wgpuLast = { frames, calls: new Map([...wgpuCalls].map(([k, v]) => [k, [...v]])) };
    return ` wgpu per frame over ${f}: ${parts.join(', ')}`;
  }

  commands() {
    return null;
  }

  dispose() {
    this.#module = null;
    this.#submit = null;
  }
}
