// The guests' GL beside a DOM desktop in the single-thread build: Mesa (its
// non-pthread build, gex/st/) in the system Worker, where every guest memory
// is, as compositor-worker.js hosts it beside the scene. Each GL window is
// presented into an OffscreenCanvas the page's desktop handed over
// (gl-relay.js), and the device's list of windows goes to the page, which
// pairs windows with frames as it does with a Mesa of its own
// (dom-compositor.js `#drawGl`).

/** `post(m, transfer)`: to the page. A guest's `gl_submit` finds the device
 *  at `__gexGlInRealm` (hosts/browser/js/worker.mjs); the first says
 *  `{ ev: 'gl-wanted' }`, for the page to start Mesa. Returns what the
 *  Worker's ops call: `start`, `canvas`, `forget`, `stop`. */
export function openGlInRealm(post) {
  let device = null;
  let ready;
  const up = new Promise((r) => (ready = r));
  let tail = Promise.resolve();
  let wanted = false;
  // Emscripten finds a canvas by selector, and a Worker has no document:
  // `#gex-gl` is the device's own, `#gex-gl-<frame>` a window's.
  const targets = new Map();
  const kept = new Map();
  let loop = 0;
  let last = '';
  // One submission at a time: the device is one wasm module, and a JSPI
  // submit that suspends must not be re-entered (gl-bridge.mjs `serveGl`).
  globalThis.__gexGlInRealm = {
    submit(memory, ring, client) {
      if (!wanted) post({ ev: 'gl-wanted' });
      wanted = true;
      const run = async () => {
        await up;
        if (!device?.ok) return -1;
        try {
          return await device.submit(memory, ring, client);
        } catch (e) {
          console.error('[gl] the device threw', e);
          return -1;
        }
      };
      const r = tail.then(run);
      tail = r.then(() => {}, () => {});
      return r;
    },
  };
  // The page's frame clock is not here: present, and say what windows the
  // device has, every 16 ms (a guest's swap waits for the first).
  const tick = () => {
    if (!device?.ok) return;
    device.display();
    const list = device.surfaces();
    const s = JSON.stringify(list);
    if (s !== last) {
      last = s;
      post({ ev: 'gl-surfaces', list });
    }
  };
  return {
    async start({ url, files, api }) {
      // The device's own canvas, where a frame no window has claimed goes, is
      // never shown: a texture (gpu-device.js `textureCanvas`), not a WebGPU
      // canvas, which on software WebGPU loses the device at its first frame.
      const [{ MesaGlDevice }, { textureCanvas }] = await Promise.all([import('./mesa-gl.js'), import('./gpu-device.js')]);
      const own = textureCanvas().canvas;
      targets.set('#gex-gl', own);
      globalThis.document ??= { querySelector: (sel) => targets.get(sel) ?? null, querySelectorAll: () => [] };
      device = await MesaGlDevice.create(own, { url, files, api: api || 'core', preserve: true });
      ready();
      if (!device.ok) return { ok: false, error: device.error || 'the GL device would not start' };
      loop = setInterval(tick, 16);
      return { ok: true, describe: device.describe() };
    },
    /** Window `frame`'s canvas, by the page's `key` for it (`canvas` the
     *  first time that key is named), or (`on` false) none. A key is kept
     *  after its frame goes: the page names the same canvas again when the
     *  window's frame changes (`showGlCanvas`), and says when it is gone. */
    canvas({ frame, on, key, canvas, gone }) {
      if (gone) return void kept.delete(gone);
      const sel = `#gex-gl-${frame}`;
      if (canvas) kept.set(key, canvas);
      if (on && kept.has(key)) targets.set(sel, kept.get(key));
      else targets.delete(sel);
      device?.surfaceCanvas(frame, on && kept.has(key));
    },
    forget(token) {
      device?.forgetClient(token);
    },
    stop() {
      clearInterval(loop);
      device?.dispose?.();
      device = null;
    },
  };
}
