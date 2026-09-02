// A desktop whose windows are elements.
//
// The other compositor (`compositor-worker.js`) draws the whole scene itself,
// in one WebGL context, on one canvas: titlebars, borders, stacking, cursor
// and all. This one draws none of that. Each window is a `<canvas>` handed to
// the page, and everything around it -- the frame, the title, where it sits,
// what happens when it is dragged -- is the page's HTML. Same compositor
// underneath: the same arena of surface descriptors, the same input ring, the
// same protocol running in each guest process.
//
// What that buys is everything the DOM already does. Decorations are CSS and
// can be anything; a window can be animated, transformed, put in a flexbox or
// a grid; the browser composites, so an idle window costs nothing where the
// WebGL scene redraws all of it every frame; and hit-testing is the browser's
// -- a `pointerdown` on a window's canvas arrives with `offsetX`/`offsetY`,
// which is surface-local coordinates, exactly, including under a transform.
//
// # Geometry
//
// Both directions, and they are not symmetrical.
//
// The page tells the compositor where it put a window (`place`). A Wayland
// toplevel does not care -- there is no position in that protocol and a client
// never learns one -- but an X client's window has real coordinates, and its
// menus, tooltips and drag-and-drop all read them, so the window manager has
// to be told or they land in the wrong place.
//
// The compositor tells the page where a window *wants* to be (`onplace`). A
// popup carries an `xdg_positioner`, an X override-redirect window carries
// coordinates its client chose, and a subsurface is placed relative to its
// parent. Those are the client's decision, not the page's.
//
// # Pixels
//
// A surface is an address in a `SharedArrayBuffer`, which is the same thing
// the WebGL compositor uploads with `texSubImage2D`. Here it goes into a 2D
// canvas, which costs a copy and a swizzle: shm is BGRA and `ImageData` is
// RGBA, and `ImageData` refuses a shared buffer besides. Damage-bounded, so a
// terminal's cursor blink copies a few hundred bytes.
//
// A window whose pixels are the host's OpenGL (see `F_EXTERNAL`) has no
// address to copy from: its frames are a texture in the worker that runs the
// guest's GL. Such a window gets its canvas handed to that worker instead --
// see `hostGlWindow` below -- and is drawn there.

import {
  readScene,
  ackBlock,
  sendEvent,
  setOutputSize,
  EV_MOTION,
  EV_BUTTON,
  EV_AXIS,
  EV_KEY,
  EV_MODS,
  EV_POINTER_CLEAR,
  EV_KEYBOARD_FOCUS,
  EV_CONFIGURE,
  EV_CLOSE,
  EV_DISMISS_POPUP,
  EV_MOVED,
  ROLE_TOPLEVEL,
  ROLE_POPUP,
  ROLE_SUBSURFACE,
  ROLE_CURSOR,
} from '@gex/wl-bridge.mjs';

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

const MOD_SHIFT = 1;
const MOD_LOCK = 2;
const MOD_CONTROL = 4;
const MOD_MOD1 = 8;
const MOD_MOD4 = 64;

/** Browser mouse buttons (0 left, 1 middle, 2 right) to Linux `BTN_*` codes. */
const BTN = { 0: 0x110, 1: 0x112, 2: 0x111 };

const key = (s) => `${s.token}:${s.client}:${s.surface}`;

/** One window, as the page sees it. */
class DomWindow {
  constructor(desktop, k, s) {
    this.key = k;
    this.token = s.token;
    this.block = s.block;
    this.client = s.client;
    this.surface = s.surface;
    this.role = s.role;
    this.parent = s.parent;
    this.width = s.width;
    this.height = s.height;
    this.title = s.title;
    /** 'server' for a window the page should put a frame around, 'none' for
     * a popup or a subsurface, which are placed and never decorated.
     *
     * Every toplevel is server-side here: the compositor answers
     * `zxdg_decoration_manager_v1` that way, so Qt and KDE draw nothing and
     * the page's HTML is the frame. GTK draws its own inside that anyway --
     * there is no signal for it, and no compositor gets one. */
    this.decorations = s.role === ROLE_TOPLEVEL ? 'server' : 'none';
    /** True when the pixels are the host's OpenGL rather than this record's
     * memory: `canvas` is then drawn by whoever runs that GL. */
    this.external = !!s.external;
    /** The X window this is, or zero. */
    this.xid = s.xid || 0;
    this.canvas = document.createElement('canvas');
    this.canvas.width = Math.max(1, s.width);
    this.canvas.height = Math.max(1, s.height);
    this.canvas.style.display = 'block';
    this.canvas.style.touchAction = 'none';
    // The client draws its own cursor; a page that wants the system one can
    // override this in CSS.
    this.canvas.style.cursor = 'none';
    this.x = 0;
    this.y = 0;
    this.relX = s.relX;
    this.relY = s.relY;
    this.serial = -1;
    this.onretitle = null;
    this.onresized = null;
    this.onplace = null;
    this.onclosed = null;
    this.#desktop = desktop;
    this.#ctx = this.canvas.getContext('2d', { alpha: true });
  }

  #desktop;
  #ctx;
  #image = null;

  /** Where the page has put this window, in output coordinates.
   *
   * A no-op for a Wayland toplevel and required for an X one: see the note at
   * the top. Call it after moving the element, including while dragging. */
  place(x, y) {
    this.x = Math.round(x);
    this.y = Math.round(y);
    this.#desktop.moved(this);
  }

  /** Ask the client to be this size. It answers by drawing at it. */
  resize(w, h) {
    this.#desktop.configure(this, Math.max(1, Math.round(w)), Math.max(1, Math.round(h)));
  }

  /** Ask the client to close, the way a titlebar button does. */
  close() {
    this.#desktop.close(this);
  }

  /** Give this window the keyboard. */
  focus() {
    this.#desktop.focus(this);
  }

  /** Draw one damaged rectangle out of the guest's memory. */
  paint(mem, s) {
    const { x, y, w, h } = clampDamage(s, this.canvas.width, this.canvas.height);
    if (w <= 0 || h <= 0) return;
    if (!this.#image || this.#image.width !== w || this.#image.height !== h) {
      this.#image = this.#ctx.createImageData(w, h);
    }
    const out = this.#image.data;
    const stride = s.stride;
    let o = 0;
    for (let row = 0; row < h; row++) {
      let i = s.addr + (y + row) * stride + x * 4;
      for (let col = 0; col < w; col++, i += 4, o += 4) {
        // shm is ARGB8888 little-endian -- B, G, R, A in memory -- and
        // `ImageData` is R, G, B, A. One swizzle, over the damage only.
        out[o] = mem[i + 2];
        out[o + 1] = mem[i + 1];
        out[o + 2] = mem[i];
        out[o + 3] = s.format === 1 ? 255 : mem[i + 3];
      }
    }
    this.#ctx.putImageData(this.#image, x, y);
  }
}

/** The damaged rectangle, clipped to what the canvas actually is. */
function clampDamage(s, cw, ch) {
  let { x, y, w, h } = s.damage ?? { x: 0, y: 0, w: s.width, h: s.height };
  if (w <= 0 || h <= 0) return { x: 0, y: 0, w: 0, h: 0 };
  x = Math.max(0, Math.min(x, cw));
  y = Math.max(0, Math.min(y, ch));
  w = Math.min(w, cw - x);
  h = Math.min(h, ch - y);
  return { x, y, w, h };
}

/**
 * A desktop of DOM windows.
 *
 * `mount` is the element windows are reported for; nothing is appended to it
 * by this class -- the page decides where each window's canvas goes, and
 * `mount` is only what keyboard and wheel events are listened on.
 */
export class DomDesktop {
  #channel;
  #mount;
  #windows = new Map();
  #memories = new Map();
  #focus = null;
  #pointerOver = null;
  #raf = 0;
  #running = false;
  #cursor = null;

  /** Called with each new window. The page keeps it and places its canvas. */
  onwindow = null;
  /** Called when a window goes away, after its `onclosed`. */
  onwindowgone = null;

  static create(mount, channel, { width, height } = {}) {
    const d = new DomDesktop();
    d.#mount = mount;
    d.#channel = channel;
    const w = width || mount.clientWidth || 1280;
    const h = height || mount.clientHeight || 760;
    setOutputSize(channel, w, h);
    d.#listen();
    d.#running = true;
    d.#raf = requestAnimationFrame(() => d.#frame());
    return d;
  }

  /** A guest process's memory, so its surfaces can be read. */
  addMemory(token, memory) {
    this.#memories.set(token, new Uint8Array(memory.buffer));
  }

  dropMemory(token) {
    this.#memories.delete(token);
  }

  stop() {
    this.#running = false;
    if (this.#raf) cancelAnimationFrame(this.#raf);
    this.#raf = 0;
    for (const win of this.#windows.values()) win.onclosed?.();
    this.#windows.clear();
  }

  /** Every window currently on the desktop, newest last. */
  windows() {
    return [...this.#windows.values()];
  }

  // -- the frame ----------------------------------------------------------

  #frame() {
    if (!this.#running) return;
    try {
      this.#reconcile();
    } catch (e) {
      console.error('[dom-desktop]', e);
    }
    this.#raf = requestAnimationFrame(() => this.#frame());
  }

  #reconcile() {
    const { surfaces, blocks } = readScene(this.#channel);
    const live = new Set();
    for (const s of surfaces) {
      const k = key(s);
      live.add(k);
      let win = this.#windows.get(k);
      if (!win) {
        win = new DomWindow(this, k, s);
        this.#windows.set(k, win);
        this.#attach(win);
        if (s.role === ROLE_CURSOR) this.#cursor = win;
        else this.onwindow?.(win);
        if (s.role === ROLE_TOPLEVEL) this.focus(win);
      }
      this.#update(win, s);
    }
    for (const [k, win] of [...this.#windows]) {
      if (live.has(k)) continue;
      this.#windows.delete(k);
      if (this.#focus === win) this.#focus = null;
      if (this.#cursor === win) this.#cursor = null;
      win.onclosed?.();
      this.onwindowgone?.(win);
    }
    for (const b of blocks) ackBlock(this.#channel, b.block, b.gen);
  }

  #update(win, s) {
    if (s.title !== win.title) {
      win.title = s.title;
      win.onretitle?.(s.title);
    }
    if (s.width !== win.width || s.height !== win.height) {
      win.width = s.width;
      win.height = s.height;
      win.canvas.width = Math.max(1, s.width);
      win.canvas.height = Math.max(1, s.height);
      win.onresized?.(s.width, s.height);
    }
    // A popup, a subsurface or an X menu is placed by its client, not by the
    // page: say where it asked to be.
    if (s.role !== ROLE_TOPLEVEL && (s.relX !== win.relX || s.relY !== win.relY)) {
      win.relX = s.relX;
      win.relY = s.relY;
      win.onplace?.(s.relX, s.relY, win.parent);
    }
    if (win.external || !s.addr) return;
    if (s.serial === win.serial) return;
    win.serial = s.serial;
    const mem = this.#memories.get(s.token);
    if (mem) win.paint(mem, s);
  }

  // -- what the page asks for ---------------------------------------------

  moved(win) {
    sendEvent(this.#channel, win.block, EV_MOVED, win.x, win.y, 0, 0,
              win.client, win.surface);
  }

  configure(win, w, h) {
    sendEvent(this.#channel, win.block, EV_CONFIGURE, w, h, 0,
              this.#focus === win ? 1 : 0, win.client, win.surface);
  }

  close(win) {
    if (win.role === ROLE_POPUP) {
      sendEvent(this.#channel, win.block, EV_DISMISS_POPUP, 0, 0, 0, 0,
                win.client, win.surface);
      return;
    }
    sendEvent(this.#channel, win.block, EV_CLOSE, 0, 0, 0, 0, win.client, win.surface);
  }

  focus(win) {
    if (this.#focus === win) return;
    const old = this.#focus;
    this.#focus = win;
    if (old) {
      sendEvent(this.#channel, old.block, EV_KEYBOARD_FOCUS, 0, 0, 0, 0, old.client, 0);
      this.configure(old, old.width, old.height);
    }
    sendEvent(this.#channel, win.block, EV_KEYBOARD_FOCUS, 0, 0, 0, 0,
              win.client, win.surface);
    this.configure(win, win.width, win.height);
  }

  // -- input --------------------------------------------------------------

  /** Every window's canvas listens for itself, which is what makes the
   *  browser's hit-testing ours: `offsetX`/`offsetY` is surface-local. */
  #attach(win) {
    const c = win.canvas;
    const at = (e) => ({
      x: Math.max(0, Math.min(win.width - 1, Math.round(e.offsetX))),
      y: Math.max(0, Math.min(win.height - 1, Math.round(e.offsetY))),
      t: Math.round(e.timeStamp),
    });
    c.addEventListener('pointerenter', (e) => {
      this.#pointerOver = win;
      const p = at(e);
      sendEvent(this.#channel, win.block, EV_MOTION, p.x, p.y, p.t, 0,
                win.client, win.surface);
    });
    c.addEventListener('pointerleave', () => {
      if (this.#pointerOver !== win) return;
      this.#pointerOver = null;
      sendEvent(this.#channel, win.block, EV_POINTER_CLEAR, 0, 0, 0, 0, win.client, 0);
    });
    c.addEventListener('pointermove', (e) => {
      const p = at(e);
      sendEvent(this.#channel, win.block, EV_MOTION, p.x, p.y, p.t, 0,
                win.client, win.surface);
    });
    c.addEventListener('pointerdown', (e) => {
      const p = at(e);
      this.focus(win);
      c.setPointerCapture?.(e.pointerId);
      sendEvent(this.#channel, win.block, EV_MOTION, p.x, p.y, p.t, 0,
                win.client, win.surface);
      sendEvent(this.#channel, win.block, EV_BUTTON, BTN[e.button] ?? 0x110, 1,
                p.t, 0, win.client, win.surface);
    });
    c.addEventListener('pointerup', (e) => {
      const p = at(e);
      c.releasePointerCapture?.(e.pointerId);
      sendEvent(this.#channel, win.block, EV_BUTTON, BTN[e.button] ?? 0x110, 0,
                p.t, 0, win.client, win.surface);
    });
    c.addEventListener('wheel', (e) => {
      e.preventDefault();
      // Wayland's axis is a fixed-point number of surface units; the browser's
      // is pixels (or lines, which are about sixteen of them).
      const scale = e.deltaMode === 1 ? 16 : 1;
      const send = (axis, v) => {
        if (!v) return;
        sendEvent(this.#channel, win.block, EV_AXIS, axis, Math.round(v * scale * 256),
                  Math.round(e.timeStamp), 0, win.client, win.surface);
      };
      send(0, e.deltaY);
      send(1, e.deltaX);
    }, { passive: false });
  }

  /** The keyboard belongs to the desktop, not to a canvas: a page may put its
   *  focus anywhere -- a titlebar's close button, say -- and the client with
   *  the keyboard should keep receiving keys. */
  #listen() {
    const mods = (e) =>
      (e.shiftKey ? MOD_SHIFT : 0) | (e.ctrlKey ? MOD_CONTROL : 0) |
      (e.altKey ? MOD_MOD1 : 0) | (e.metaKey ? MOD_MOD4 : 0) |
      (e.getModifierState?.('CapsLock') ? MOD_LOCK : 0);
    const onKey = (e, pressed) => {
      const win = this.#focus;
      if (!win) return;
      const code = EVDEV[e.code];
      if (code === undefined) return;
      e.preventDefault();
      sendEvent(this.#channel, win.block, EV_MODS, mods(e), 0, 0, 0, win.client, 0);
      sendEvent(this.#channel, win.block, EV_KEY, code, pressed ? 1 : 0,
                Math.round(e.timeStamp), 0, win.client, win.surface);
    };
    const down = (e) => onKey(e, true);
    const up = (e) => onKey(e, false);
    this.#mount.addEventListener('keydown', down);
    this.#mount.addEventListener('keyup', up);
    // A desktop takes the keyboard, so it has to be focusable.
    if (!this.#mount.hasAttribute('tabindex')) this.#mount.tabIndex = 0;
    this.#detach = () => {
      this.#mount.removeEventListener('keydown', down);
      this.#mount.removeEventListener('keyup', up);
    };
  }

  #detach = null;

  /** The cursor surface a client asked for, if it has one: a page that wants
   *  to draw it puts this canvas at the pointer, offset by its hotspot. */
  cursor() {
    return this.#cursor;
  }
}

export { ROLE_TOPLEVEL, ROLE_POPUP, ROLE_SUBSURFACE, ROLE_CURSOR };
