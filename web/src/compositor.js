// The page's side of gex-wayland's compositor: it owns nothing but the
// listeners.
//
// The output canvas is handed to a worker (`compositor-worker.js`) with
// `transferControlToOffscreen`, so drawing never runs on the page's event
// loop -- a session with a dozen windows redrawing keeps the terminal, the
// scrollbar and the dropdown responsive. What stays here is what only the page
// can do: DOM events, and the canvas's size.

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

  static async create(canvas, channel, inputTarget = canvas) {
    const c = new Compositor();
    await c.#start(canvas, channel, inputTarget);
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
      this.#worker.postMessage({ kind: 'gl', channel, ...opts });
    });
  }

  async #start(canvas, channel, inputTarget) {
    this.#canvas = canvas;
    this.#stats = new SharedArrayBuffer(16);
    const worker = new Worker(new URL('./compositor-worker.js', import.meta.url), {
      type: 'module',
    });
    this.#worker = worker;
    const offscreen = canvas.transferControlToOffscreen();
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
      if (m.kind === 'describe') {
        this.#pending.get(m.id)?.(m.state);
        this.#pending.delete(m.id);
      } else if (m.kind === 'error') {
        console.error('[compositor]', m.error);
      }
    });
    worker.postMessage(
      { kind: 'init', canvas: offscreen, channel, stats: this.#stats },
      [offscreen],
    );
    await ready;
    this.#listen(inputTarget);
  }

  /** Where a DOM event lands in output pixels. */
  #at(e) {
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
      target.focus?.();
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
      // 10 units a notch, in Wayland's axis sign (down is positive).
      const dy = Math.sign(e.deltaY) * 10;
      const dx = Math.sign(e.deltaX) * 10;
      if (dy) post({ kind: 'pointer-axis', axis: 0, value: dy, time: now() });
      if (dx) post({ kind: 'pointer-axis', axis: 1, value: dx, time: now() });
      e.preventDefault();
    };
    const onKey = (pressed) => (e) => {
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
  addMemory(token, memory) {
    this.#worker.postMessage({ kind: 'memory', token, memory });
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
  describe() {
    const id = this.#next++;
    return new Promise((resolve) => {
      this.#pending.set(id, resolve);
      this.#worker.postMessage({ kind: 'describe', id });
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
