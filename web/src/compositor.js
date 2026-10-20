// The page's side of gex-wayland's compositor: it owns nothing but the
// listeners.
//
// The output canvas is handed to a worker (`compositor-worker.js`) with
// `transferControlToOffscreen`, so drawing never runs on the page's event
// loop -- a session with a dozen windows redrawing keeps the terminal, the
// scrollbar and the dropdown responsive. What stays here is what only the page
// can do: DOM events, and the canvas's size.

import { assetUrl, pageParams } from './page-env.js';

/** `KeyboardEvent.code` -> Linux evdev keycode, which is what Wayland carries. */
const EVDEV = {
  Escape: 1, Digit1: 2, Digit2: 3, Digit3: 4, Digit4: 5, Digit5: 6, Digit6: 7,
  Digit7: 8, Digit8: 9, Digit9: 10, Digit0: 11, Minus: 12, Equal: 13,
  Backspace: 14, Tab: 15, KeyQ: 16, KeyW: 17, KeyE: 18, KeyR: 19, KeyT: 20,
  KeyY: 21, KeyU: 22, KeyI: 23, KeyO: 24, KeyP: 25, BracketLeft: 26,
  BracketRight: 27, Enter: 28, ControlLeft: 29, KeyA: 30, KeyS: 31, KeyD: 32,
  KeyF: 33, KeyG: 34, KeyH: 35, KeyJ: 36, KeyK: 37, KeyL: 38, Semicolon: 39,
  Quote: 40, Backquote: 41, ShiftLeft: 42, Backslash: 43, KeyZ: 44, KeyX: 45,
  KeyC: 46, KeyV: 47, KeyB: 48, KeyN: 49, KeyM: 50, Comma: 51, Period: 52,
  Slash: 53, ShiftRight: 54, NumpadMultiply: 55, AltLeft: 56, Space: 57,
  CapsLock: 58, F1: 59, F2: 60, F3: 61, F4: 62, F5: 63, F6: 64, F7: 65, F8: 66,
  F9: 67, F10: 68, F11: 87, F12: 88, ControlRight: 97, AltRight: 100,
  Home: 102, ArrowUp: 103, PageUp: 104, ArrowLeft: 105, ArrowRight: 106,
  End: 107, ArrowDown: 108, PageDown: 109, Insert: 110, Delete: 111,
  MetaLeft: 125, MetaRight: 126,
};

// xkb modifier-mask bits, matching the "complete" types the keymap includes.
const MOD_SHIFT = 1;
const MOD_LOCK = 2; // Caps
const MOD_CONTROL = 4;
const MOD_MOD1 = 8; // Alt
const MOD_MOD4 = 64; // Super

/** Browser mouse buttons (0 left, 1 middle, 2 right) to Linux `BTN_*` codes. */
const BTN = { 0: 0x110, 1: 0x112, 2: 0x111 };

export class Compositor {
  #worker;
  #canvas;
  #detach = [];
  #pending = new Map();
  #next = 1;
  #stats;

  /** `makeWorker`: what stands in for compositor-worker.js's Worker (the
   *  single-thread build's system Worker hosts it; see runtime.js).
   *  `scale`: the page's devicePixelRatio when the canvas's backing store is
   *  in device pixels (the frames and titles the compositor draws are scaled
   *  by it). */
  static async create(canvas, channel, inputTarget = canvas, { makeWorker = null, scale = 1 } = {}) {
    const c = new Compositor();
    await c.#start(canvas, channel, inputTarget, makeWorker, { scale });
    return c;
  }

  /** Run the guest's OpenGL in the compositor's own worker.
   *
   * Not on the page: the frames have to be textures in the context that draws
   * the scene, or a GL window costs a readback and an upload every frame. The
   * ring is the same shared one the page-side device would have served. */
  hostGl(channel, opts = {}) {
    return new Promise((resolve, reject) => {
      const onMessage = ({ data: m }) => {
        if (m.kind !== 'gl-ready' && m.kind !== 'gl-failed') return;
        this.#worker.removeEventListener('message', onMessage);
        if (m.kind === 'gl-ready') resolve(m.describe);
        else reject(new Error(m.error));
      };
      this.#worker.addEventListener('message', onMessage);
      // `?mesaenv=K=V,K=V`: environment for Mesa in the GL worker
      // (MESA_DEBUG=incomplete_fbo, say), for finding what it refuses.
      // `?glcapture=BYTES`: record the GL stream in the worker and POST it
      // to /__gex_capture when the ceiling is reached (gl-capture.js).
      const q = pageParams();
      const mesaEnv = q?.get('mesaenv');
      const glCapture = q?.get('glcapture');
      // `?wgslcache=all|site|0`: which shader translations Mesa starts with
      // (mesa-gl.js #loadWgslCache).
      const wgslCache = q?.get('wgslcache');
      // `?wgpustats=1`: count Mesa's WebGPU calls for the stats line
      // (mesa-gl.js countWebGpu); off by default, it costs time.
      const wgpuStats = q?.get('wgpustats') === '1';
      this.#worker.postMessage({ kind: 'gl', channel, url: assetUrl('/gex/gex-wgpu-replay.js'), ...opts, ...(mesaEnv ? { mesaEnv } : {}),
        ...(glCapture ? { glCapture } : {}), ...(wgslCache ? { wgslCache } : {}),
        ...(wgpuStats ? { wgpuStats } : {}) });
    });
  }

  async #start(canvas, channel, inputTarget, makeWorker, { scale = 1 } = {}) {
    const sameTarget = inputTarget === canvas;
    // A canvas can be handed to a worker once and never again, and a page
    // that runs a session twice hands over the same element the second time:
    // `transferControlToOffscreen` then throws an InvalidStateError before
    // anything has started, which reads as the compositor being broken rather
    // than as this element having been used up. It has: put a fresh one in
    // its place and hand that over.
    let offscreen;
    try {
      offscreen = canvas.transferControlToOffscreen();
    } catch (e) {
      canvas = freshCanvas(canvas);
      if (sameTarget) inputTarget = canvas;
      offscreen = canvas.transferControlToOffscreen();
    }
    this.#canvas = canvas;
    // Without a SharedArrayBuffer (the single-thread build) the counters are
    // a copy `makeWorker`'s stand-in keeps current.
    this.#stats = typeof SharedArrayBuffer === 'function' ? new SharedArrayBuffer(16) : new ArrayBuffer(16);
    const worker = makeWorker
      ? makeWorker()
      : new Worker(new URL('./compositor-worker.js', import.meta.url), { type: 'module' });
    this.#worker = worker;
    const ready = new Promise((resolve, reject) => {
      const onMessage = ({ data: m }) => {
        if (m.kind === 'ready') {
          worker.removeEventListener('message', onMessage);
          resolve();
        } else if (m.kind === 'error') {
          worker.removeEventListener('message', onMessage);
          reject(new Error(m.error));
        }
      };
      worker.addEventListener('message', onMessage);
    });
    worker.addEventListener('message', ({ data: m }) => {
      if (m.kind === 'direct') {
        this.#setDirect(m.w ? { w: m.w, h: m.h } : null);
      } else if (m.kind === 'cursor') {
        // The client's cursor, as the browser's own: one pointer, never a
        // second one drawn into the frame.
        this.#canvas.style.cursor = m.css;
        this.#cursorCss = m.css;
      } else if (m.kind === 'describe' || m.kind === 'wgsl-cache' || m.kind === 'readout') {
        this.#pending.get(m.id)?.(m.kind === 'wgsl-cache' ? { blob: m.blob, stats: m.stats } : m.state);
        this.#pending.delete(m.id);
      } else if (m.kind === 'error') {
        console.error('[compositor]', m.error);
      }
    });
    // `?present=canvas|readback`: how the scene reaches the canvas (see
    // presentMode in compositor-worker.js); by default, by the adapter.
    const pq = pageParams();
    const present = pq?.get('present') ?? null;
    // `?direct=1`: direct present (compositor-worker.js): a single GL window
    // is the canvas, fitted to the viewport; compositing only when needed.
    const direct = pq?.get('direct') === '1';
    this.#pointerLock = pq?.get('pointerlock') === '1';
    // `?gllimits=40|41|43`: the device asks for WebGPU's default limits,
    // GL 4.1's texture limits, or those and the adapter's compute limits too
    // (the default), which decide the GL version Mesa's context earns
    // (gpu-device.js).
    const glLimits = pq?.has('gllimits') ? Number(pq.get('gllimits')) || 0 : null;
    worker.postMessage(
      { kind: 'init', canvas: offscreen, width: canvas.width, height: canvas.height, scale,
        channel, stats: this.#stats, present, direct, glLimits },
      [offscreen],
    );
    await ready;
    this.#listen(inputTarget);
  }

  /** Where a DOM event lands in output pixels. */
  // -- direct present (page side) ----------------------------------------
  #direct = null;
  #savedStyle = null;
  #cursorCss = 'none';
  #pointerLock = false;
  #locked = null; // the virtual pointer while the pointer is locked

  /** The GL frame became the canvas (`d` = its size) or stopped being it.
   *  Fitted to the viewport, letterboxed, centred. */
  #setDirect(d) {
    const c = this.#canvas;
    if (d && !this.#direct) {
      this.#savedStyle = c.getAttribute('style');
      Object.assign(c.style, { width: '100vw', height: '100vh', objectFit: 'contain', background: '#000',
        position: 'fixed', left: '0', top: '0', cursor: this.#cursorCss });
    } else if (!d && this.#direct) {
      if (this.#savedStyle === null) c.removeAttribute('style');
      else c.setAttribute('style', this.#savedStyle);
      if (document.pointerLockElement === c) document.exitPointerLock?.();
    }
    this.#direct = d;
  }

  #at(e) {
    if (this.#direct) {
      const d = this.#direct;
      if (this.#locked && document.pointerLockElement === this.#canvas) {
        // Locked: a virtual pointer moved by the deltas, kept in the frame.
        this.#locked.x = Math.max(0, Math.min(d.w - 1, this.#locked.x + (e.movementX || 0)));
        this.#locked.y = Math.max(0, Math.min(d.h - 1, this.#locked.y + (e.movementY || 0)));
        return { x: Math.round(this.#locked.x), y: Math.round(this.#locked.y) };
      }
      // object-fit: contain -- the frame's box inside the element's.
      const r = this.#canvas.getBoundingClientRect();
      const k = Math.min(r.width / d.w, r.height / d.h) || 1;
      const left = r.left + (r.width - d.w * k) / 2;
      const top = r.top + (r.height - d.h * k) / 2;
      const p = { x: Math.round((e.clientX - left) / k), y: Math.round((e.clientY - top) / k) };
      this.#locked = p;
      return p;
    }
    const r = this.#canvas.getBoundingClientRect();
    const sx = this.#canvas.width / Math.max(1, r.width);
    const sy = this.#canvas.height / Math.max(1, r.height);
    return {
      x: Math.round((e.clientX - r.left) * sx),
      y: Math.round((e.clientY - r.top) * sy),
    };
  }

  #listen(target) {
    const post = (m) => this.#worker.postMessage(m);
    const now = () => performance.now() | 0;
    const mods = (e) =>
      (e.shiftKey ? MOD_SHIFT : 0) |
      (e.ctrlKey ? MOD_CONTROL : 0) |
      (e.altKey ? MOD_MOD1 : 0) |
      (e.metaKey ? MOD_MOD4 : 0) |
      (e.getModifierState?.('CapsLock') ? MOD_LOCK : 0);

    const onMove = (e) => {
      const { x, y } = this.#at(e);
      post({ kind: 'pointer-move', x, y, time: now() });
    };
    const onDown = (e) => {
      // Without preventScroll, focusing scrolls the canvas into view when the
      // page is shorter than the canvas and whatever sits above it (the eject
      // page's 34 px bar), and #at below then measures the canvas where it is
      // *after* the scroll: the press lands 34 px below where it was made.
      // FL's menu row and 5D Chess's buttons both missed this way.
      target.focus?.({ preventScroll: true });
      // `?pointerlock=1` and direct present: the first click locks the pointer.
      if (this.#direct && this.#pointerLock && document.pointerLockElement !== target) target.requestPointerLock?.();
      const button = BTN[e.button];
      if (button === undefined) return;
      const { x, y } = this.#at(e);
      post({ kind: 'pointer-move', x, y, time: now() });
      post({ kind: 'pointer-button', button, pressed: true, time: now() });
      e.preventDefault();
    };
    // On the window, because a drag that ends off the canvas still ends: a
    // titlebar drag that released outside used to leave the window stuck to
    // the pointer.
    const onUp = (e) => {
      const button = BTN[e.button];
      if (button === undefined) return;
      post({ kind: 'pointer-button', button, pressed: false, time: now() });
    };
    const onWheel = (e) => {
      // A wl_fixed number of pixels (1/256 px), in Wayland's axis sign (down
      // is positive). The compositor makes wheel notches of it at 100 px a
      // notch (gex-wayland WHEEL_PX, Chromium's deltaY for one): a line is a
      // third of that, a page the canvas. A trackpad's small deltas add up there.
      const scale = 256 * (e.deltaMode === 1 ? 100 / 3 : e.deltaMode === 2 ? (target.clientHeight || 600) : 1);
      const dy = Math.round(e.deltaY * scale);
      const dx = Math.round(e.deltaX * scale);
      if (dy) post({ kind: 'pointer-axis', axis: 0, value: dy, time: now() });
      if (dx) post({ kind: 'pointer-axis', axis: 1, value: dx, time: now() });
      e.preventDefault();
    };
    const onKey = (pressed) => (e) => {
      // Alt+Enter (direct present): the canvas full screen, as games have it.
      if (pressed && this.#direct && e.altKey && e.code === 'Enter') {
        if (document.fullscreenElement) document.exitFullscreen?.();
        else target.requestFullscreen?.();
        e.preventDefault();
        return;
      }
      const code = EVDEV[e.code];
      if (code === undefined) return;
      post({ kind: 'key', code, pressed, mods: mods(e), time: now() });
      e.preventDefault();
    };
    const onCtx = (e) => e.preventDefault(); // right-click is a button here
    const onDown2 = onKey(true);
    const onUp2 = onKey(false);

    if (!target.hasAttribute?.('tabindex')) target.setAttribute?.('tabindex', '0');
    target.addEventListener('mousemove', onMove);
    target.addEventListener('mousedown', onDown);
    window.addEventListener('mouseup', onUp);
    target.addEventListener('wheel', onWheel, { passive: false });
    target.addEventListener('contextmenu', onCtx);
    target.addEventListener('keydown', onDown2);
    target.addEventListener('keyup', onUp2);
    this.#detach.push(() => {
      target.removeEventListener('mousemove', onMove);
      target.removeEventListener('mousedown', onDown);
      window.removeEventListener('mouseup', onUp);
      target.removeEventListener('wheel', onWheel);
      target.removeEventListener('contextmenu', onCtx);
      target.removeEventListener('keydown', onDown2);
      target.removeEventListener('keyup', onUp2);
    });
  }

  /** A guest process's memory: where its surfaces' pixels are. */
  addMemory(token, memory, kernel) {
    this.#worker.postMessage({ kind: 'memory', token, memory, kernel });
  }

  /** That process has exited: its memory, its GL context and its GL windows
   *  go (compositor-worker.js 'drop-memory'). */
  dropMemory(token) {
    this.#worker.postMessage({ kind: 'drop-memory', token });
  }

  resize(width, height) {
    this.#worker.postMessage({ kind: 'resize', width, height });
  }

  /** Frames drawn and windows mapped, without a round trip. */
  counters() {
    const a = new Int32Array(this.#stats);
    return { frames: Atomics.load(a, 0), windows: Atomics.load(a, 1) };
  }

  /** The scene as the compositor sees it: window titles, focus, upload path. */
  /** Mesa's shader translation cache: { blob (ArrayBuffer), stats }. */
  wgslCache() {
    const id = this.#next++;
    return new Promise((resolve) => {
      this.#pending.set(id, resolve);
      this.#worker.postMessage({ kind: 'wgsl-cache', id });
    });
  }

  /** The composited output read back: `{ w, h, px }` (RGBA, top row first), or null. */
  readout() {
    const id = this.#next++;
    return new Promise((resolve) => {
      this.#pending.set(id, resolve);
      this.#worker.postMessage({ kind: 'readout', id });
      setTimeout(() => {
        if (this.#pending.delete(id)) resolve(null);
      }, 5000);
    });
  }

  /** The output read back as base64 RGBA, for a harness: `{ w, h, b64 }` or null. */
  async readoutBase64() {
    const r = await this.readout();
    if (!r) return null;
    let s = '';
    const px = r.px;
    for (let i = 0; i < px.length; i += 0x8000) s += String.fromCharCode.apply(null, px.subarray(i, i + 0x8000));
    return { w: r.w, h: r.h, b64: btoa(s) };
  }

  /** `{ pixels: false }`: everything but `lit`/`brightest`/`hash`, which
   *  read the whole output back from the GPU (a full-scene copy and map). */
  describe({ pixels = true } = {}) {
    const id = this.#next++;
    return new Promise((resolve) => {
      this.#pending.set(id, resolve);
      this.#worker.postMessage({ kind: 'describe', id, pixels });
      setTimeout(() => {
        if (this.#pending.delete(id)) resolve(null);
      }, 2000);
    });
  }

  stop() {
    for (const off of this.#detach) off();
    this.#detach = [];
    try {
      this.#worker.postMessage({ kind: 'stop' });
      this.#worker.terminate();
    } catch {
      /* already gone */
    }
    if (globalThis.__gexCompositor === this) delete globalThis.__gexCompositor;
  }
}

/** A canvas exactly like this one, in its place in the page.
 *
 * Everything a compositor's output needs carried over: the pixel size it is
 * drawn at, the id and classes a page's CSS and a harness both look it up by,
 * whatever inline style it was given, and its place in the tab order -- so
 * the replacement is the same element as far as the rest of the page is
 * concerned. The old one is left with nothing referring to it.
 */
function freshCanvas(old) {
  const fresh = document.createElement('canvas');
  fresh.width = old.width;
  fresh.height = old.height;
  fresh.id = old.id;
  fresh.className = old.className;
  if (old.hasAttribute('style')) fresh.setAttribute('style', old.getAttribute('style'));
  if (old.hasAttribute('tabindex')) fresh.setAttribute('tabindex', old.getAttribute('tabindex'));
  if (old.parentNode) old.replaceWith(fresh);
  return fresh;
}
