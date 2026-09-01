/*
 * !!! MobileGL IS DISABLED. MobileGlDevice.create() ALWAYS THROWS. !!!
 *
 * There is no flag, token, query parameter or environment variable that turns
 * it back on, by design. Do not add one, and do not run it "just as a
 * comparison" -- it is not a valid comparison. Re-enabling it means deleting
 * the throw in create(), which is a deliberate change visible in a diff.
 *
 * It does not render SuperTuxKart -- it does not draw the race at all, and it
 * is not a "mostly working" backend with a bug. Do not use it as a reference,
 * a control, or a differential baseline when debugging the Mesa/gex_wgpu path.
 * Reasoning of the form "MobileGL renders this so the defect is Mesa-specific"
 * is unsound and has produced wrong conclusions before.
 *
 * The ONLY working reference path for STK is virgl: the `virgl` / `wayland`
 * demos, GALLIUM_DRIVER=virpipe into virglrenderer. Use that when you need to
 * know what a correct frame looks like or how far the application really gets.
 *
 * Nothing runs this any more -- every demo and gate is on Mesa. The file
 * remains only because deleting the fork and its build plumbing is more
 * hassle than it is worth, not because anything depends on it.
 */
// Browser half of gex's direct GL wire driver: guest GLES calls are restored
// on MobileGL's WebGPU backend. This deliberately has no Mesa/virgl layer.
import { MAGIC, PROTOCOL_VERSION, HEADER_BYTES, error, op } from './gl-proto.js';
import { captureIfRequested } from './gl-capture.js';

const OP_NAME = new Map(Object.entries(op).map(([name, value]) => [value, name]));

const H_MAGIC = 0;
const H_CAPACITY = 1;
const H_TAIL = 2;
const H_REPLY_LEN = 4;
const H_STATUS = 5;
const H_FAILED_AT = 6;
const H_PROTOCOL_VERSION = 7;

async function loadGlue(url) {
  const res = await fetch(url, { cache: 'no-store' });
  if (!res.ok) throw new Error(`${url}: ${res.status} ${res.statusText}`);
  const blob = URL.createObjectURL(new Blob([await res.text()], { type: 'text/javascript' }));
  try {
    return (await import(/* @vite-ignore */ blob)).default;
  } finally {
    URL.revokeObjectURL(blob);
  }
}

async function requestDevice() {
  if (!navigator.gpu) throw new Error('this browser does not expose WebGPU');
  const adapter = await navigator.gpu.requestAdapter({ powerPreference: 'high-performance' });
  if (!adapter) throw new Error('WebGPU did not provide an adapter');
  const requiredFeatures = [
    'rg11b10ufloat-renderable',
    'float32-filterable',
    'depth32float-stencil8',
    'readonly-and-readwrite-storage-textures',
  ]
    .filter((feature) => adapter.features.has(feature));
  const requiredLimits = {};
  if (adapter.limits.maxColorAttachmentBytesPerSample > 32) {
    requiredLimits.maxColorAttachmentBytesPerSample =
      adapter.limits.maxColorAttachmentBytesPerSample;
  }
  return adapter.requestDevice({ requiredFeatures, requiredLimits });
}

export class MobileGlDevice {
  #module = null;
  #submit = null;
  #worker = null;
  #stats = null;
  #failed = null;
  #description = '';
  #state = '';
  #commandState = '';
  #channelId = 0;
  #requestId = 0;
  #pending = new Map();
  #recent = [];
  #batchEnds = new Map();
  #batchTimes = new Map();
  #trace = false;
  #submits = 0;
  #bytes = 0;
  #submitMs = 0;
  #maxSubmitMs = 0;

  // The single definition of "this run asked for MobileGL". Both selectors are
  // live: `webgpu=1` is the historical one every gl44 gate used, `gl=mobilegl`
  // is the one that reads like it should work. They used to mean different
  // things -- `gl=mobilegl` selected neither backend and quietly fell through
  // to plain WebGL -- which is exactly the kind of drift this collapses.
  // Callers use this to *report* an ignored request, never to honour one.
  static requested(query) {
    return query.get('gl') === 'mobilegl' || query.get('webgpu') === '1';
  }

  static async create(canvas, url = '/gex/gexmobilegl-webgpu.mjs') {
    // Unconditional. There is deliberately no flag, token, or environment
    // variable that turns this back on: MobileGL is worthless even as a
    // comparison, so an escape hatch would only ever be used by mistake or by
    // a future agent talking itself into "just as a control". The banner above
    // was already here and stopped nothing -- the gl44 gates defaulted to this
    // backend and ran it unnoticed. If MobileGL is ever genuinely needed, the
    // change is deleting this throw, which is a deliberate act that shows up
    // in a diff.
    throw new Error(
      'MobileGL is disabled and has no override. It does not render ' +
      'SuperTuxKart and is not a valid reference for the Mesa/gex_wgpu ' +
      'path, not even as a comparison. Use gl=mesa, or virgl when you need ' +
      'to know what a correct frame looks like.');
    /* eslint-disable no-unreachable */
    const out = new MobileGlDevice();
    canvas.id = 'gex-gl';
    out.capture = captureIfRequested(PROTOCOL_VERSION, canvas.width, canvas.height);
    try {
      const device = await requestDevice();
      device.addEventListener('uncapturederror', (event) => {
        console.error('[mobilegl] WebGPU validation:', event.error?.message || event.error);
      });
      const Module = await loadGlue(url);
      const artifactVersion = Date.now().toString(36);
      out.#module = await Module({
        locateFile: (file) => {
          const artifact = new URL(file, new URL(url, location.href));
          artifact.searchParams.set('gex-build', artifactVersion);
          return artifact.href;
        },
        preinitializedWebGPUDevice: device,
        dumpWgsl: Number(new URLSearchParams(location.search).get('dumpwgsl') || 0),
        printErr: (s) => console.warn('[mobilegl]', s),
        print: (s) => console.log('[mobilegl]', s),
      });
      out.#submit = WebAssembly.promising(out.#module._gex_mobilegl_submit);
      const rc = out.#module._gex_mobilegl_init(canvas.width, canvas.height);
      if (rc !== 0) out.#failed = `MobileGL initialization failed (${rc})`;
    } catch (e) {
      out.#failed = e?.message || String(e);
    }
    return out;
  }

  async #createWorker(canvas, url) {
    const worker = new Worker(new URL('./mobilegl-worker.js', import.meta.url), { type: 'module' });
    this.#worker = worker;
    // displayed, produced. Keep them separate: the first is the user-visible
    // cadence while the second diagnoses a guest that is not swapping.
    this.#stats = new Int32Array(new SharedArrayBuffer(32));
    const ready = new Promise((resolve, reject) => {
      const fail = (e) => reject(new Error(e?.message || 'the MobileGL renderer worker failed to start'));
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
          this.#failed = m.error || 'the MobileGL renderer worker failed';
        }
      });
    });
    const offscreen = canvas.transferControlToOffscreen();
    worker.postMessage({
      kind: 'init',
      canvas: offscreen,
      url,
      stats: this.#stats.buffer,
    }, [offscreen]);
    try {
      await ready;
    } catch (e) {
      this.#failed = e?.message || String(e);
      worker.terminate();
      this.#worker = null;
    }
  }

  get ok() { return (this.#module !== null || this.#worker !== null) && this.#failed === null; }
  get error() { return this.#failed; }
  describe() {
    if (!this.ok) return this.#failed;
    return this.#worker ? this.#description : 'MobileGL DirectWebGPU (direct gex GL transport)';
  }
  display() {
    if (this.#worker) {
      // Request one queued frame for this browser refresh. The renderer worker
      // observes the serial from its command-serving loop, which avoids both
      // main-thread GL replay and a Worker rAF that can be microtask-starved.
      Atomics.add(this.#stats, 5, 1);
      Atomics.notify(this.#stats, 5);
      return this.presentedCount();
    }
    return this.ok && this.#module._gex_mobilegl_display
      ? this.#module._gex_mobilegl_display()
      : this.presentedCount();
  }
  presentedCount() {
    if (!this.ok) return 0;
    return this.#worker ? Atomics.load(this.#stats, 0) : this.#module._gex_mobilegl_presented();
  }
  producedCount() {
    if (this.#worker) return this.ok ? Atomics.load(this.#stats, 1) : 0;
    return this.ok && this.#module._gex_mobilegl_produced
      ? this.#module._gex_mobilegl_produced()
      : this.presentedCount();
  }
  glState() {
    if (this.#worker) {
      if (!this.ok) return this.#failed;
      const counts = `presented=${this.presentedCount()} produced=${this.producedCount()}`;
      const submits = Atomics.load(this.#stats, 2);
      const totalUs = Atomics.load(this.#stats, 3);
      const maxUs = Atomics.load(this.#stats, 4);
      const timing = ` worker=1 submits=${submits} submit_ms=${(totalUs / 1000).toFixed(1)}` +
        ` avg_ms=${submits ? (totalUs / submits / 1000).toFixed(3) : '0.000'}` +
        ` max_ms=${(maxUs / 1000).toFixed(1)}`;
      return /^presented=\d+ produced=\d+/.test(this.#state)
        ? this.#state.replace(/^presented=\d+ produced=\d+[^|]*/, `${counts}${timing} `)
        : `${counts}${timing}${this.#state ? ` | ${this.#state}` : ''}`;
    }
    const avg = this.#submits ? this.#submitMs / this.#submits : 0;
    return `presented=${this.presentedCount()} produced=${this.producedCount()} direct=1 submits=${this.#submits} ` +
      `bytes=${this.#bytes} submit_ms=${this.#submitMs.toFixed(1)} ` +
      `avg_ms=${avg.toFixed(3)} max_ms=${this.#maxSubmitMs.toFixed(1)}`;
  }
  commands() {
    if (this.#worker) return this.ok ? this.#commandState : this.#failed;
    const tail = this.#recent.length ? ` | recent: ${this.#recent.join(' ')}` : '';
    const batches = [...this.#batchEnds.entries()]
      .sort((a, b) => b[1] - a[1])
      .slice(0, 12)
      .map(([name, count]) => `${name}:${count}`)
      .join(' ');
    const times = [...this.#batchTimes.entries()]
      .sort((a, b) => b[1].total - a[1].total)
      .slice(0, 8)
      .map(([name, value]) =>
        `${name}:${value.total.toFixed(0)}ms/${value.count}` +
        `=${(value.total / value.count).toFixed(3)} max=${value.max.toFixed(1)}`)
      .join(' ');
    return `gex-gl -> MobileGL -> WebGPU${tail}${batches ? ` | batch ends: ${batches}` : ''}` +
      `${times ? ` | batch time: ${times}` : ''}`;
  }
  traceCommands(enabled = true) {
    if (this.#worker) {
      this.#worker.postMessage({ kind: 'trace', on: Boolean(enabled) });
      return;
    }
    this.#trace = Boolean(enabled);
  }
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
  dispose() {
    this.#worker?.terminate();
    this.#worker = null;
    for (const done of this.#pending.values()) done();
    this.#pending.clear();
  }

  // Give the renderer worker the GL ring directly. The page only owns the
  // lightweight wake-up fallback; no command decoding runs on its event loop.
  attachChannel(channel) {
    if (!this.#worker) return null;
    const id = ++this.#channelId;
    channel.direct = typeof Atomics.waitAsync === 'function';
    this.#worker.postMessage({ kind: 'channel', id, channel });
    return channel.direct
      ? () => {}
      : () => this.#worker?.postMessage({ kind: 'serve', id });
  }

  // Same interface as GlDevice, so the existing SharedArrayBuffer bridge and
  // guest /dev/gex-gl device need no backend-specific behavior.
  submit(memory, ring) {
    if (!this.ok) return -1;
    const words = new Uint32Array(memory.buffer, ring, HEADER_BYTES / 4);
    if (words[H_MAGIC] !== MAGIC) return -1;
    if (words[H_PROTOCOL_VERSION] !== PROTOCOL_VERSION) {
      words[H_STATUS] = error.VERSION_MISMATCH;
      return -1;
    }
    const capacity = words[H_CAPACITY];
    const tail = words[H_TAIL];
    if (tail > capacity) return -1;

    const started = performance.now();
    this.#submits++;
    this.#bytes += tail;
    const src = new Uint8Array(memory.buffer, ring + HEADER_BYTES, tail);
    // Recorded before execution, for the same reason the native device does:
    // the batch that breaks the backend is the one a replay needs.
    this.capture?.record(src, words[3]);
    let lastOpcode = null;
    if (this.#trace) {
      const view = new DataView(src.buffer, src.byteOffset, src.byteLength);
      for (let at = 0; at + 8 <= src.byteLength;) {
        const opcode = view.getUint32(at, true);
        const size = view.getUint32(at + 4, true);
        if (size < 8 || at + size > src.byteLength) break;
        this.#recent.push(OP_NAME.get(opcode) || String(opcode));
        if (this.#recent.length > 24) this.#recent.shift();
        lastOpcode = opcode;
        at += size;
      }
      if (lastOpcode !== null) {
        const name = OP_NAME.get(lastOpcode) || String(lastOpcode);
        this.#batchEnds.set(name, (this.#batchEnds.get(name) || 0) + 1);
      }
    }
    const ptr = this.#module._malloc(tail || 1);
    this.#module.HEAPU8.set(src, ptr);
    const finish = (rc) => {
      const elapsed = performance.now() - started;
      this.#submitMs += elapsed;
      this.#maxSubmitMs = Math.max(this.#maxSubmitMs, elapsed);
      if (this.#trace && lastOpcode !== null) {
        const name = OP_NAME.get(lastOpcode) || String(lastOpcode);
        const value = this.#batchTimes.get(name) || { total: 0, count: 0, max: 0 };
        value.total += elapsed;
        value.count++;
        value.max = Math.max(value.max, elapsed);
        this.#batchTimes.set(name, value);
      }
      this.#module._free(ptr);
      if (rc !== 0) {
        // Decoder failure is terminal.  A command may have executed a valid
        // prefix before discovering a malformed tail, so neither its reply
        // nor this GL context is safe to reuse.
        this.#failed = `GL transport decoder rejected a batch (${rc})`;
        words[H_STATUS] = error.BAD_OPCODE;
        words[H_REPLY_LEN] = 0;
        words[H_TAIL] = 0;
        words[H_FAILED_AT] = 0;
        return rc;
      }
      const replyLen = this.#module._gex_mobilegl_reply_len();
      const replyCap = words[3];
      if (replyLen > replyCap) {
        words[H_STATUS] = error.BAD_ARGUMENT;
        words[H_REPLY_LEN] = 0;
        words[H_TAIL] = 0;
        return -1;
      }
      if (replyLen) {
        const from = this.#module._gex_mobilegl_reply_data();
        new Uint8Array(memory.buffer, ring + HEADER_BYTES + capacity, replyLen)
          .set(this.#module.HEAPU8.subarray(from, from + replyLen));
      }
      words[H_TAIL] = 0;
      words[H_REPLY_LEN] = replyLen;
      words[H_STATUS] = error.NONE;
      words[H_FAILED_AT] = 0;
      return rc;
    };
    const result = this.#submit(ptr, tail);
    return result instanceof Promise ? result.then(finish) : finish(result);
  }
}
