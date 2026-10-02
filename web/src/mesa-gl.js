// The guest's GL device, backed by Mesa's frontend and the gex_wgpu Gallium
// driver on the browser's WebGPU.
//
// Same role as `GlDevice` (WebGL 2): it is handed the
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
  /** The virglrenderer this Mesa encodes for, when that is the backend. */
  #renderer = null;
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
   * The same Mesa, on the driver that has always drawn these pixels.
   *
   * Gallium's virgl encoder speaks the vtest protocol to virglrenderer, which
   * here is a module in this same context rather than a process across a
   * socket: `Module.gexVirgl` is what the winsys's redirected `socket`,
   * `read` and `write` end up calling (see the Mesa fork's `gex_vtest_os.c`).
   * The renderer owns the canvas and blits what it last drew onto it, so this
   * backend never presents anything itself.
   */
  static async #onVirgl(canvas, { url, api, major, minor, preserve }) {
    const out = new MesaGlDevice();
    try {
      const { VirglRenderer } = await import('./virgl.js');
      // In this context on purpose: the winsys calls across synchronously,
      // from inside a wasm frame, with nothing to await on.
      const renderer = await VirglRenderer.create(canvas, '/gex/gexvirgl.mjs', null,
                                                  { inProcess: true, preserve });
      // Frames are asked for rather than noticed: this Mesa is on this side
      // of the transport and knows when a frame is finished and whose it is.
      renderer.presentDirect?.(true);
      if (!renderer.ok) throw new Error(renderer.describe?.() || 'the renderer would not start');
      out.#renderer = renderer;

      const Module = await loadGlue(url);
      out.#module = await Module({
        mainScriptUrlOrBlob: new URL(url, location.href).href,
        locateFile: (file) => new URL(file, new URL(url, location.href)).href,
        print: (line) => console.log('[mesa-virgl]', line),
        printErr: (line) => console.warn('[mesa-virgl]', line),
      });
      // Before `init`, which is what opens the connection.
      // `?gltrace=1` narrates the vtest exchange. A stream protocol that
      // stops says nothing on its own: the last command written and whether
      // a reply followed is the whole diagnosis.
      const trace = new URLSearchParams(location.search).get('gltrace') === '1';
      const head = (b) => (b.length >= 8
        ? `len=${new DataView(b.buffer, b.byteOffset).getUint32(0, true)} ` +
          `cmd=${new DataView(b.buffer, b.byteOffset).getUint32(4, true)}`
        : `${b.length} bytes`);
      out.#module.gexVirgl = {
        open: () => {
          const s = renderer.openSession();
          if (trace) console.log('[vtest] open ->', s);
          return s;
        },
        write: (session, bytes) => {
          const n = renderer.feed(session, bytes);
          if (trace) console.log('[vtest] write', bytes.length, head(bytes), '->', n,
                                 'pending', renderer.pending(session));
          return n;
        },
        read: (session, out8) => {
          const n = renderer.drainInto(session, out8);
          if (trace) console.log('[vtest] read want', out8.length, '->', n);
          return n;
        },
        close: (session) => renderer.close(session),
        // Which window the frame about to be flushed belongs to. Said by the
        // decoder, one module over, because only it knows -- and needed by
        // the renderer, which keeps a frame per window.
        surface: (uid) => renderer.presentSurface(uid),
        // And that this surface's frame is finished: the renderer takes its
        // copy then and only then, from the framebuffer that is still bound.
        snapshot: (uid, resource) => renderer.snapshotSurface(uid, resource),
      };
      // No JSPI here: virglrenderer runs the commands before `feed` returns,
      // so a submit is an ordinary call.
      out.#submit = (ptr, len, client) =>
        out.#module._gex_gl_submit(ptr, len, client >>> 0);
      if (out.#module._gex_gl_set_api)
        out.#module._gex_gl_set_api(api === 'core' ? 1 : 0, major, minor);
      const rc = out.#module._gex_gl_init(canvas.width, canvas.height);
      if (rc !== 0) out.#failed = `Mesa/virgl initialization failed (${rc})`;
      out.#description = `Mesa st/mesa + virgl on virglrenderer (${api})`;
      out.capture = captureIfRequested(PROTOCOL_VERSION, canvas.width, canvas.height);
    } catch (e) {
      out.#failed = e?.message || String(e);
    }
    return out;
  }

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
      // The backend is the choice; the module follows from it. Naming the
      // module still works and still decides the backend when it is all a
      // caller passes, but asking for a backend and getting the other one's
      // module -- which is what a url-shaped default did -- cannot happen.
      backend = (options.url || '').includes('virgl') ? 'virgl' : 'wgpu',
      url = backend === 'virgl' ? '/gex/gex-virgl-replay.js' : '/gex/gex-wgpu-replay.js',
    } = options;
    if (backend === 'virgl') {
      return MesaGlDevice.#onVirgl(canvas, { url, api, major, minor, preserve: options.preserve });
    }
    const out = new MesaGlDevice();
    canvas.id = 'gex-gl';
    try {
      if (!navigator.gpu) throw new Error('navigator.gpu unavailable');
      const adapter = await navigator.gpu.requestAdapter({ powerPreference: 'high-performance' });
      if (!adapter) throw new Error('WebGPU adapter unavailable');
      out.#adapter = adapter;
      const optional = ['texture-compression-bc', 'depth32float-stencil8',
                        'rg11b10ufloat-renderable', 'depth-clip-control',
                      'float32-filterable', 'float32-blendable',
                        'clip-distances', 'dual-source-blending',
                        'timestamp-query'];
      // Each of these gates a GL extension st/mesa derives from format
      // support: BC compression, ARB_depth_buffer_float, EXT_packed_float,
    // and ARB_depth_clamp. clip-distances is not a format feature -- it is
      // what lets a translated shader declare `enable clip_distances`, which
      // is how gl_ClipDistance reaches WGSL at all.

      // Absent ones are simply not requested; the driver asks the device what
      // it actually got rather than assuming.
      const requiredFeatures = optional.filter((f) => adapter.features.has(f));
      // A device gets WebGPU's *default* limits unless it asks for more, and
      // those defaults are well below what this adapter can do -- storage
      // buffers in the vertex stage default to none at all. The driver reads
      // the limits back from the device and sizes its caps from them, so
      // anything not asked for here is capability the backend never sees.
      //
      // st/mesa needs 13 constant buffers per stage for
      // ARB_uniform_buffer_object, which is GL 3.1; WebGPU guarantees only 12,
      // so the driver backs the rest with storage buffers. Ask for everything
      // up to what the driver is built for, clamped to the adapter --
      // requesting above the adapter's maximum makes requestDevice reject.
      const want = {
        maxUniformBuffersPerShaderStage: 16,
        maxStorageBuffersPerShaderStage: 16,
        maxStorageBuffersInVertexStage: 16,
        maxStorageBuffersInFragmentStage: 16,
        // Eight RGBA8 draw buffers is exactly WebGPU's default 32 bytes per
        // sample, so any wider attachment set fails on it -- GL 3.2 wants
        // eight draw buffers of whatever format the application chose.
        maxColorAttachmentBytesPerSample: 64,
      };
      const requiredLimits = {};
      for (const [limit, wanted] of Object.entries(want)) {
        const available = adapter.limits[limit];
        if (typeof available === 'number' && available > 0)
          requiredLimits[limit] = Math.min(available, wanted);
      }
      const device = await adapter.requestDevice({ requiredFeatures, requiredLimits });
      out.#device = device;
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
      (globalThis.__gexWebGPU ||= []).push({ adapter, device, queue: device.queue });
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
        printErr: (line) => console.warn('[mesa-gl]', line),
      });

      // Readback suspends the wasm stack, so submission goes through JSPI.
      out.#submit = WebAssembly.promising(out.#module.gexGlSubmitRaw);
      if (out.#module._gex_gl_set_api)
        out.#module._gex_gl_set_api(api === 'core' ? 1 : 0, major, minor);
      const rc = out.#module._gex_gl_init(canvas.width, canvas.height);
      if (rc !== 0) out.#failed = `Mesa/WebGPU initialization failed (${rc})`;
      out.#description = `Mesa st/mesa + gex_wgpu on WebGPU (${api})`;
      out.capture = captureIfRequested(PROTOCOL_VERSION, canvas.width, canvas.height);
    } catch (e) {
      out.#failed = e?.message || String(e);
    }
    return out;
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
    this.capture?.record(src, replyCap);

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

  /** The renderer this device encodes for, when the backend is virgl. */
  get renderer() {
    return this.#renderer;
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
    // The frames first: after the client is gone its surfaces are no longer
    // enumerable, so this is the last chance to know which they were.
    if (this.#renderer) {
      for (const s of this.surfaces()) {
        if (s.client === client && s.frame) this.#renderer.forgetSurface(s.frame);
      }
    }
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
      `readbacks=${this.#readbacks} readback_ms=${this.#readbackMs.toFixed(1)}`;
  }

  commands() {
    return null;
  }

  dispose() {
    this.#module = null;
    this.#submit = null;
  }
}
