// The page's half of the guest's GPU.
//
// The guest runs Debian's own Mesa with its Gallium `virgl` driver, which
// speaks the vtest protocol over a UNIX socket. `shims/browser/src/virgl.rs`
// serves that socket and hands the bytes here; here hands them to
// virglrenderer compiled to wasm. The default build targets WebGL 2; the
// compatibility build routes its desktop GL calls through MobileGL to WebGPU.
// It is a second module -- a copy in, a copy out, the canvas
// -- so virglrenderer allocates in its own heap and never in the guest's
// address space, which the shim shares to read syscall buffers directly.

/**
 * Load the renderer's emscripten glue. It is a build artifact in `public/`, so
 * importing it directly fails on the dev server (a rule about source, which
 * this is not); fetched as text and turned into a module instead, with
 * `locateFile` naming where its wasm is.
 */
async function loadGlue(url) {
  const res = await fetch(url);
  if (!res.ok) throw new Error(`${url}: ${res.status} ${res.statusText}`);
  const blob = URL.createObjectURL(
    new Blob([await res.text()], { type: 'text/javascript' }),
  );
  try {
    return (await import(/* @vite-ignore */ blob)).default;
  } finally {
    URL.revokeObjectURL(blob);
  }
}

async function webGpuDeviceFor(url) {
  if (!url.includes('webgpu')) return null;
  if (!navigator.gpu) throw new Error('this browser does not expose WebGPU');
  const adapter = await navigator.gpu.requestAdapter({ powerPreference: 'high-performance' });
  if (!adapter) throw new Error('WebGPU did not provide an adapter');
  const optional = [
    'rg11b10ufloat-renderable',
    'float32-filterable',
    'texture-compression-bc',
  ]
    .filter((feature) => adapter.features.has(feature));
  const requiredLimits = {};
  // WebGPU's portable default is only 32 bytes/sample across all color
  // attachments. Desktop GL exposes eight render targets, and MobileGL must
  // sometimes materialize all of them for a shader even when the application
  // routes some outputs to GL_NONE. Ask for the adapter's advertised budget
  // instead of silently accepting the smaller default device limit.
  if (adapter.limits.maxColorAttachmentBytesPerSample > 32) {
    requiredLimits.maxColorAttachmentBytesPerSample =
      adapter.limits.maxColorAttachmentBytesPerSample;
  }
  return adapter.requestDevice({ requiredFeatures: optional, requiredLimits });
}

export class VirglRenderer {
  #module = null;
  #pump = null;
  #worker = null;
  #stats = null;
  #canvas = null;
  #failed = null;
  #description = '';
  #state = '';
  #commandState = '';
  #channelId = 0;
  #requestId = 0;
  #pending = new Map();

  /**
   * @param {HTMLCanvasElement} canvas the drawing surface
   * @param {string} url where gexvirgl.mjs was published
   * @param {(s: string) => void} [onLog] where the renderer's diagnostics go;
   *   the console default is dropped under load in a headless browser, so a
   *   harness with its own channel should pass one.
   */
  static async create(canvas, url = '/gex/gexvirgl.mjs', onLog = null) {
    const r = new VirglRenderer();
    r.#canvas = canvas;
    // virglrenderer looks the canvas up by selector, so it has to carry the
    // id the module asks for.
    canvas.id = 'gex-gl';
    if (
      typeof Worker === 'function' &&
      typeof canvas.transferControlToOffscreen === 'function' &&
      typeof SharedArrayBuffer === 'function'
    ) {
      await r.#createWorker(canvas, url);
      return r;
    }
    try {
      const webGpuDevice = await webGpuDeviceFor(url);
      const Module = await loadGlue(url);
      r.#module = await Module({
        // Where the renderer's own wasm is, said rather than inferred. The
        // glue works it out from `import.meta.url`, which is a blob when it
        // is loaded the way `loadGlue` has to load it.
        locateFile: (file) => new URL(file, new URL(url, location.href)).href,
        // virglrenderer explains itself on stderr and says nothing through a
        // return value beyond "no", so this is the only diagnostic there is.
        printErr: (s) => (onLog ? onLog(s) : console.warn('[virgl]', s)),
        print: (s) => (onLog ? onLog(s) : console.log('[virgl]', s)),
        dumpWgsl: globalThis.__gexDumpWgsl ||
          Number(new URLSearchParams(location.search).get('dumpwgsl')) || 0,
        ...(webGpuDevice ? { preinitializedWebGPUDevice: webGpuDevice } : {}),
      });
      r.#pump = webGpuDevice
        ? WebAssembly.promising(r.#module._gex_virgl_pump)
        : r.#module._gex_virgl_pump;
      const offscreen =
        typeof OffscreenCanvas === 'function' && canvas instanceof OffscreenCanvas;
      const rc = r.#module._gex_virgl_init(canvas.width, canvas.height, offscreen ? 1 : 0);
      if (rc !== 0) {
        r.#failed = `the renderer would not start (${rc})`;
      }
    } catch (e) {
      r.#failed = e?.message || String(e);
    }
    return r;
  }

  async #createWorker(canvas, url) {
    const worker = new Worker(new URL('./virgl-worker.js', import.meta.url), { type: 'module' });
    this.#worker = worker;
    this.#stats = new Int32Array(new SharedArrayBuffer(16));
    const ready = new Promise((resolve, reject) => {
      const fail = (e) => reject(new Error(e?.message || 'the renderer worker failed to start'));
      worker.addEventListener('error', fail, { once: true });
      worker.addEventListener('message', (event) => {
        const m = event.data || {};
        if (m.kind === 'ready') {
          worker.removeEventListener('error', fail);
          this.#failed = m.error || null;
          this.#description = m.description || '';
          this.#state = m.state || '';
          this.#commandState = m.commands || '';
          resolve();
        } else if (m.kind === 'diagnostic') {
          this.#state = m.state || this.#state;
          this.#commandState = m.commands || this.#commandState;
          const done = this.#pending.get(m.id);
          if (done) {
            this.#pending.delete(m.id);
            done();
          }
        } else if (m.kind === 'error') {
          this.#failed = m.error || 'the renderer worker failed';
        }
      });
    });
    const offscreen = canvas.transferControlToOffscreen();
    worker.postMessage({
      kind: 'init',
      canvas: offscreen,
      url,
      stats: this.#stats.buffer,
      dumpWgsl: Number(new URLSearchParams(location.search).get('dumpwgsl')) || 0,
    }, [offscreen]);
    try {
      await ready;
    } catch (e) {
      this.#failed = e?.message || String(e);
      worker.terminate();
      this.#worker = null;
    }
  }

  get ok() {
    return (this.#module !== null || this.#worker !== null) && this.#failed === null;
  }

  get error() {
    return this.#failed;
  }

  /** What the guest will be told it has. */
  describe() {
    if (!this.ok) return this.#failed;
    return this.#worker
      ? this.#description
      : this.#module.UTF8ToString(this.#module._gex_virgl_describe());
  }

  /**
   * The renderer's GL state, and how many frames it put on the canvas.
   * `presented` is the one to read: vtest has no present, so a run can draw
   * perfectly with nothing visible.
   */
  glState() {
    if (!this.ok) return this.#failed;
    return this.#worker ? this.#state : this.#module.UTF8ToString(this.#module._gex_virgl_gl_state());
  }

  /** Number of completed images copied to the browser canvas. */
  presentedCount() {
    if (!this.ok) return 0;
    return this.#worker ? Atomics.load(this.#stats, 0) : this.#module._gex_virgl_presented();
  }

  /**
   * Every command the guest sent, by name and count, and every reason a
   * readback did not become a picture -- the one to read when the canvas is
   * black, since it says whether the guest drew at all.
   */
  commands() {
    if (!this.ok) return this.#failed;
    return this.#worker
      ? this.#commandState
      : this.#module.UTF8ToString(this.#module._gex_virgl_commands());
  }

  /** Ask the worker for an exact diagnostic snapshot before reporting a run. */
  refresh() {
    if (!this.#worker || !this.ok) return Promise.resolve();
    const id = ++this.#requestId;
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        this.#pending.delete(id);
        resolve();
      }, 2000);
      this.#pending.set(id, () => {
        clearTimeout(timer);
        resolve();
      });
      this.#worker.postMessage({ kind: 'diagnostic', id });
    });
  }

  /**
   * Give the renderer worker one guest bridge and return its cheap page-side
   * wake function. Shared buffers do the data/reply transfer; the page only
   * forwards a notification and never executes GL commands.
   */
  attachChannel(channel) {
    if (!this.#worker) return null;
    const id = ++this.#channelId;
    channel.direct = typeof Atomics.waitAsync === 'function';
    this.#worker.postMessage({ kind: 'channel', id, channel });
    return channel.direct
      ? () => {}
      : () => this.#worker?.postMessage({ kind: 'serve', id });
  }

  /**
   * Hand over `len` bytes of the guest's stream, at `ptr` in the *shim's*
   * memory, and run every command they complete.
   *
   * @returns {number} reply bytes waiting, or -1 with no renderer
   */
  submit(session, shimMemory, ptr, len) {
    if (!this.ok) return -1;
    const buf = this.#module._malloc(len);
    this.#module.HEAPU8.set(new Uint8Array(shimMemory.buffer, ptr, len), buf);
    this.#module._gex_virgl_feed(session, buf, len);
    this.#module._free(buf);
    const finish = (ran) => {
      // A refusal ends the connection: a stream protocol cannot resynchronise
      // after a command of unknown length.
      if (ran < 0) {
        this.#failed = 'the renderer refused the stream';
        return -1;
      }
      return this.#module._gex_virgl_pending(session);
    };
    const ran = this.#pump(session);
    return ran instanceof Promise ? ran.then(finish) : finish(ran);
  }

  /** Copy up to `max` reply bytes to `ptr` in the shim's memory. */
  take(session, shimMemory, ptr, max) {
    if (!this.ok) return 0;
    const have = this.#module._gex_virgl_pending(session);
    const n = Math.min(have, max);
    if (n > 0) {
      const from = this.#module._gex_virgl_replies(session);
      new Uint8Array(shimMemory.buffer, ptr, n).set(
        this.#module.HEAPU8.subarray(from, from + n),
      );
    }
    // All or nothing: the shim always asks for everything waiting, and a
    // partial drain would leave the rest to be re-sent on top of itself.
    this.#module._gex_virgl_drain(session);
    return n;
  }

  /**
   * Narrate every command as it starts, for when the renderer does not come
   * back: it runs on the page's thread, so a command that loops freezes
   * everything, and the last line printed names the command it went into.
   */
  traceCommands(on) {
    if (!this.ok) return;
    if (this.#worker) this.#worker.postMessage({ kind: 'trace', on: !!on });
    else this.#module._gex_virgl_trace_commands(on ? 1 : 0);
  }

  /** A client has gone: drop its context and its buffers. */
  close(session) {
    if (!this.ok) return;
    if (this.#worker) this.#worker.postMessage({ kind: 'close', session });
    else this.#module._gex_virgl_close(session);
  }

  dispose() {
    this.#worker?.terminate();
    this.#worker = null;
    for (const done of this.#pending.values()) done();
    this.#pending.clear();
  }
}

/**
 * The three imports `shims/browser/src/virgl.rs` declares.
 *
 * `memory` is the shim's, resolved lazily because the module that owns it is
 * being instantiated when these are handed over.
 */
export function virglImports(renderer, getMemory) {
  return {
    virgl_submit: (session, ptr, len) =>
      (renderer ? renderer.submit(session, getMemory(), ptr, len) : -1),
    virgl_take: (session, ptr, max) =>
      (renderer ? renderer.take(session, getMemory(), ptr, max) : 0),
    virgl_close: (session) => renderer?.close(session),
  };
}
