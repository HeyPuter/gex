// The guest's GL device, backed by Mesa's frontend and the gex_wgpu Gallium
// driver on the browser's WebGPU.
//
// Same role as `GlDevice` (WebGL 2) and `MobileGlDevice`: it is handed the
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
// is why this only ever failed on the dev server, and only on this backend:
// mobilegl.js has always gone through a blob and so never met the rule.
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
  #failed = null;
  #description = '';
  #submits = 0;
  #bytes = 0;
  // Wall time actually spent inside a batch, so "the Mesa path is slow" can be
  // attributed rather than assumed: MobileGL reports the same three numbers,
  // and the comparison is the whole point of having both backends.
  #submitMs = 0;
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
      url = '/gex/gex-wgpu-replay.js',
      api = 'es',
      major = 0,
      minor = 0,
      hostEnv = [],
    } = options;
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
        // Varyings. WebGPU guarantees sixteen and the driver used to assume
        // exactly that, which put GL_MAX_FRAGMENT_INPUT_COMPONENTS at 64
        // against a required 128 -- so a shader with more than sixteen
        // varyings did not link. Adapters offer far more: 31 on native Dawn,
        // 28 in Chromium 151. The driver reads back what the device granted
        // and sizes the GL limit from that, so asking here is the whole of
        // it; the clamp below keeps the request inside the adapter.
        maxInterStageShaderVariables: 32,
        maxStorageBuffersPerShaderStage: 16,
        maxStorageBuffersInVertexStage: 16,
        maxStorageBuffersInFragmentStage: 16,
        // Shader images. The native path asks for these in
        // gex_wgpu_native.c and this list did not ask at all, so the browser
        // got WebGPU's guaranteed *four* per stage where native gets eight --
        // a limit nobody requested looking exactly like a device wall, which
        // is the same trap the inter-stage variables were in. A test that
        // binds an image of every target at once needs more than four.
        maxStorageTexturesPerShaderStage: 8,
        maxStorageTexturesInVertexStage: 8,
        maxStorageTexturesInFragmentStage: 8,
        // Eight RGBA8 draw buffers is exactly WebGPU's default 32 bytes per
        // sample, so any wider attachment set fails on it -- GL 3.2 wants
        // eight draw buffers of whatever format the application chose.
        maxColorAttachmentBytesPerSample: 64,
        // Compute, which the native path has asked for since GL compute
        // landed and this one did not: st/mesa gates ARB_compute_shader behind
        // a granted maxComputeInvocationsPerWorkgroup of 1024 and WebGPU's
        // default is 256, so the browser was the only place the extension
        // could silently be missing.
        maxComputeInvocationsPerWorkgroup: 1024,
        maxComputeWorkgroupSizeX: 1024,
        maxComputeWorkgroupSizeY: 1024,
        maxComputeWorkgroupSizeZ: 64,
        maxComputeWorkgroupStorageSize: 32768,
        // Textures, because the reported *GL version* depends on them and
        // nothing else does: the 4.1 rung of _mesa_compute_version requires a
        // 16384 texture and 2048 array layers, where WebGPU guarantees 8192
        // and 256. A browser whose adapter offers more reports GL 4.3; one
        // that does not reports 4.0.
        maxTextureDimension1D: 16384,
        maxTextureDimension2D: 16384,
        maxTextureDimension3D: 2048,
        maxTextureArrayLayers: 2048,
      };
      const requiredLimits = {};
      const short = [];
      for (const [limit, wanted] of Object.entries(want)) {
        const available = adapter.limits[limit];
        if (typeof available === 'number' && available > 0) {
          requiredLimits[limit] = Math.min(available, wanted);
          if (available < wanted) short.push(`${limit} ${available}<${wanted}`);
        } else {
          short.push(`${limit} absent`);
        }
      }
      // Every one of these is a GL capability -- or, for the texture limits, a
      // whole GL *version* -- that the frontend will not enable, and a missing
      // extension is a poor way to find that out. The native path reports 4.4
      // and a browser on the same machine reported 4.0, which is this list and
      // nothing else: maxTextureDimension2D 8192 against the 16384 the 4.1
      // rung of _mesa_compute_version requires.
      // console.warn, not log: the test harness echoes warnings and drops
      // logs, and this is the one line that explains a whole missing GL
      // version.
      if (short.length)
        console.warn(`gex-gl: adapter is short of ${short.length} wanted ` +
                     `limit(s): ${short.join(', ')}`);
      const device = await adapter.requestDevice({ requiredFeatures, requiredLimits });
      out.#device = device;
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
      // Host-side environment, before anything reads it. Every driver
      // diagnostic is getenv-gated and getenv answers NULL in the browser
      // without this, so the instruments existed everywhere except the place
      // they were needed.
      if (hostEnv.length && out.#module._gex_gl_setenv) {
        const encoder = new TextEncoder();
        for (const pair of hostEnv) {
          const bytes = encoder.encode(`${pair}\0`);
          const ptr = out.#module._malloc(bytes.length);
          if (!ptr) continue;
          out.#module.HEAPU8.set(bytes, ptr);
          out.#module._gex_gl_setenv(ptr);
          out.#module._free(ptr);
        }
        console.warn('[mesa-gl] host env:', hostEnv.join(' '));
      }
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

  /** Run one batch. `memory` holds the ring; `ring` is its offset. */
  submit(memory, ring) {
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
      if (elapsed > this.#maxSubmitMs) this.#maxSubmitMs = elapsed;
      this.#module._free(ptr);
      if (rc !== 0) {
        // Decoder failure is terminal: a command may have executed a valid
        // prefix before its malformed tail was found, so neither the reply nor
        // this GL context is safe to reuse.
        this.#failed = `Mesa/WebGPU decoder rejected a batch (${rc})`;
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
      result = this.#submit(ptr, tail);
    } catch (e) {
      return report(e);
    }
    return result instanceof Promise
      ? result.then(done, report)
      : done(result);
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

  // The rest of the renderer interface MobileGlDevice provides. That backend
  // runs its GL in a worker and answers these from shared state; this one
  // decodes on the page's own event loop, so a present is simply a call that
  // has returned and there is no worker to talk to. Answering honestly here
  // is what lets a directGl demo -- a real application, not a demo written
  // for this driver -- run against either backend unchanged.
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
    return `presented=${this.presentedCount()} produced=${this.producedCount()} ` +
      `mesa=1 submits=${this.#submits} bytes=${this.#bytes} ` +
      `submit_ms=${this.#submitMs.toFixed(1)} ` +
      `avg_ms=${(this.#submitMs / Math.max(1, this.#submits)).toFixed(3)} ` +
      `max_ms=${this.#maxSubmitMs.toFixed(1)}`;
  }

  commands() {
    return null;
  }

  dispose() {
    this.#module = null;
    this.#submit = null;
  }
}
