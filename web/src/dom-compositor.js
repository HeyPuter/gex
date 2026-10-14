// A desktop whose windows are elements.
//
// The other compositor (`compositor-worker.js`) draws the whole scene itself,
// in one WebGPU scene, on one canvas: titlebars, borders, stacking, cursor
// and all. This one draws none of that. Each window is a `<canvas>` handed to
// the page, and everything around it -- the frame, the title, where it sits,
// what happens when it is dragged -- is the page's HTML. Same compositor
// underneath: the same arena of surface descriptors, the same input ring, the
// same protocol running in each guest process.
//
// What that buys is everything the DOM already does. Decorations are CSS and
// can be anything; a window can be animated, transformed, put in a flexbox or
// a grid; the browser composites, so an idle window costs nothing where the
// WebGPU scene redraws all of it every frame; and hit-testing is the browser's
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
// the WebGPU compositor uploads with `queue.writeTexture`. Here it goes into a 2D
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
  setKernelMemory,
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
    this.decorations = s.role === ROLE_TOPLEVEL && !(s.placed && s.unmanaged) ? 'server' : 'none';
    /** Placed by its client (`gex_window_placement_v1`: a Win32 window, an X
     * menu): `relX`/`relY` are where it goes in the desktop's pixels, and
     * `layer` (0 normal, 1 topmost, 2 menus) and `onrestack` how it stacks.
     * The page does not move it. */
    this.placed = !!s.placed;
    this.layer = s.layer || 0;
    this.stackId = s.stackId || 0;
    this.restack = 0;
    this.activate = 0;
    this.onrestack = null;
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
    // Not `cursor: none`: the client's cursor surface is applied to the
    // desktop as a CSS cursor and inherited from there, so hiding the
    // pointer over a window would hide the client's own cursor with it.
    // See `DomDesktop.#showCursor`.
    this.x = 0;
    this.y = 0;
    this.relX = s.relX;
    this.relY = s.relY;
    this.serial = -1;
    /** How many times this window has been painted from guest memory. */
    this.paints = 0;
    this.onretitle = null;
    this.onresized = null;
    this.onplace = null;
    this.onclosed = null;
    this.#desktop = desktop;
  }

  #desktop;
  #ctx = null;
  #image = null;
  #warned = false;

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

  /** Copy this window's OpenGL frame in, from wherever the renderer put it.
   *
   * `source` is the renderer's own canvas with this window's frame in its
   * top-left corner, at `w` by `h`; the copy is a canvas-to-canvas draw,
   * which the browser does on the GPU. Scaled only when the window and the
   * frame disagree about the size, which happens for a frame or two after a
   * resize. */
  showGl(source, w, h) {
    if (w <= 0 || h <= 0) return;
    const ctx = this.#context();
    ctx.drawImage(source, 0, 0, w, h,
                  0, 0, this.canvas.width, this.canvas.height);
  }

  /** The 2D context, taken on first paint rather than at construction.
   *
   * A canvas gets one context and keeps it. A window whose pixels are the
   * host's GL needs that one context to be WebGPU, so taking a 2D
   * one up front would quietly make every GL window unpresentable. */
  #context() {
    if (!this.#ctx) this.#ctx = this.canvas.getContext('2d', { alpha: true });
    return this.#ctx;
  }

  /** Draw one damaged rectangle out of the guest's memory, or all of it. */
  paint(mem, s, full = false) {
    this.paints++;
    const { x, y, w, h } = clampDamage(s, this.canvas.width, this.canvas.height, full);
    if (w <= 0 || h <= 0) return;
    // The descriptor and the memory can disagree -- a client's bad buffer, or
    // a view taken before the memory grew. Reading past the end would paint a
    // window full of zeroes and say nothing about why.
    const need = s.addr + (y + h - 1) * s.stride + (x + w) * 4;
    if (need > mem.length) {
      if (!this.#warned) {
        this.#warned = true;
        console.warn(`[dom-desktop] ${this.title}: buffer ends at ${need}, memory is ${mem.length}`);
      }
      return;
    }
    const ctx = this.#context();
    if (!this.#image || this.#image.width !== w || this.#image.height !== h) {
      this.#image = ctx.createImageData(w, h);
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
    ctx.putImageData(this.#image, x, y);
  }
}

/** The damaged rectangle, clipped to what the canvas actually is.
 *
 * `full` -- or damage that names no area -- means the whole surface. */
function clampDamage(s, cw, ch, full = false) {
  const named = !full && s.damage && s.damage.w > 0 && s.damage.h > 0;
  let { x, y, w, h } = named ? s.damage : { x: 0, y: 0, w: s.width, h: s.height };
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
  #records = [];
  #silent = new Set();
  #cursorShown = -1;

  /** Called with each new window. The page keeps it and places its canvas. */
  onwindow = null;
  /** The guest's OpenGL, once a window that needs it turns up. */
  #gl = null;
  #glCanvas = null;
  #glServed = false;
  #glServe = null;
  /* The window frames already copied, by serial, so a still picture is
   * copied once rather than every frame. */
  #glShown = new Map();
  #glList = [];
  #glLooks = 0;
  #glCopies = 0;
  #glGone = false;
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

  /** The output's size from now on (app.js `resize()`): what a client asks
   *  for the screen's size sees this. Windows already up keep their places. */
  resizeOutput(w, h) {
    setOutputSize(this.#channel, Math.max(1, Math.round(w)), Math.max(1, Math.round(h)));
  }

  /** Run the guest's OpenGL for this desktop.
   *
   * The device is built here and now, before any client has drawn: the guest's
   * first GL call is an ioctl that waits for an answer, and a device built
   * later than that is a thirty-second stall and a trap.
   *
   * It draws on a canvas of this desktop's own, which is never shown: it is
   * where the renderer puts whichever window's frame is being asked for, and
   * each window's canvas is copied out of it (see `#drawGl`). That is one
   * canvas-to-canvas copy per window per frame -- on the GPU, with no
   * readback -- and it is what lets several GL windows be drawn at once by
   * one renderer.
   */
  async hostGl(channel, opts = {}) {
    const canvas = document.createElement('canvas');
    // In the document, because both backends look this canvas up with
    // `document.querySelector` and one that is not in it is simply not found
    // -- a renderer that will not start, rather than a canvas that stays
    // blank. And off to the side rather than `display: none`, because a
    // WebGPU context on a canvas that is not rendered does not survive:
    // Chromium destroyed the device in the same breath as the first present,
    // with nothing calling `destroy()` and every reference held, and from
    // then on every GL window was black while the client went on drawing.
    // The same backend on an OffscreenCanvas in a worker -- what the other
    // compositor uses -- never lost a device once.
    canvas.style.position = 'absolute';
    canvas.style.top = '0';
    canvas.style.left = '-99999px';
    canvas.style.pointerEvents = 'none';
    // As large as the desktop: every window's frame is staged here, so it has
    // to hold the biggest of them.
    canvas.width = Math.max(640, this.#mount.clientWidth || 1280);
    canvas.height = Math.max(480, this.#mount.clientHeight || 760);
    canvas.style.touchAction = 'none';
    this.#mount.append(canvas);
    this.#glCanvas = canvas;
    const { MesaGlDevice } = await import('./mesa-gl.js');
    const device = await MesaGlDevice.create(canvas, {
      url: opts.url,
      api: opts.api || 'core',
      // This canvas is composited by the page, so keep what was presented
      // into it: a client that draws slower than the page composites would
      // otherwise show black between its own frames.
      preserve: true,
    });
    if (!device.ok) {
      this.onglfailed?.(device.error);
      throw new Error(device.error || 'the GL device would not start');
    }
    this.#gl = device;
    // Presented straight onto that canvas, which `#drawGl` copies into the
    // window's own.
    const { serveGl } = await import('@gex/gl-bridge.mjs');
    this.#glServe = serveGl(device, channel);
    this.#glServed = true;
    this.ongl?.(device.describe());
    return device.describe();
  }

  /** Told when the guest's OpenGL is up, or would not start. */
  ongl = null;
  onglfailed = null;

  /** What this desktop has done about the host's OpenGL: how often it looked,
   *  how many frames it copied into a window, and what its list of windows
   *  says. For diagnosis -- a window that stays black is either not being
   *  looked at, not matched, or matched and drawn from an empty frame. */
  glStats() {
    return {
      looks: this.#glLooks,
      copies: this.#glCopies,
      list: this.#glList.length,
      served: this.#glServed,
    };
  }

  /** The GL device, when one is running: what it presents and how much of it,
   *  which is the only way to tell a black frame from a frame that never
   *  arrived. */
  get gl() {
    return this.#gl;
  }

  /** A guest process's memory, so its surfaces can be read.
   *
   * The `WebAssembly.Memory` is kept, not a view of it: growing it replaces
   * the buffer, and a view taken at startup then covers only the first few
   * megabytes of a process that has grown to hundreds. Every surface past
   * that point reads as zero -- a window that is simply blank, with nothing
   * to say for itself. */
  addMemory(token, memory, kernel) {
    this.#memories.set(token, { memory, view: new Uint8Array(memory.buffer) });
    setKernelMemory(this.#channel, kernel);
  }

  /** The current view of a process's memory, re-derived if it has grown. */
  #memoryOf(token) {
    const held = this.#memories.get(token);
    if (!held) return null;
    if (held.view.buffer !== held.memory.buffer) {
      held.view = new Uint8Array(held.memory.buffer);
    }
    return held.view;
  }

  dropMemory(token) {
    this.#memories.delete(token);
    // And whatever that process had on the host's GL: its context, its
    // windows and their frames. Nothing else will say so -- a client that
    // exits is not there to send a detach.
    this.#gl?.forgetClient?.(token);
  }

  stop() {
    this.#running = false;
    if (this.#raf) cancelAnimationFrame(this.#raf);
    this.#raf = 0;
    for (const win of this.#windows.values()) win.onclosed?.();
    this.#windows.clear();
    // The guest's OpenGL goes with it. Left running, a finished session keeps
    // a Mesa module, a WebGPU context and a thread waiting on a ring
    // nobody writes to any more -- and the *next* session then asks the
    // browser for a second set of all of it, which is how a run that works
    // the first time throws the second.
    this.#glServe?.stop?.();
    this.#glServe = null;
    this.#glServed = false;
    const gl = this.#gl;
    this.#gl = null;
    this.#glShown.clear();
    this.#glList = [];
    gl?.dispose?.();
    const canvas = this.#glCanvas;
    this.#glCanvas = null;
    canvas?.remove();
  }

  /** The surface records this desktop last read, as the compositor saw them.
   *
   * For diagnosis: a window that stays blank is either a record with nothing
   * in it -- no memory, no damage -- or a paint that went wrong, and only the
   * record says which. */
  records() {
    return this.#records;
  }

  /** Every window currently on the desktop, newest last. */
  windows() {
    return [...this.#windows.values()];
  }

  // -- the frame ----------------------------------------------------------

  #frame() {
    if (!this.#running) return;
    try {
      // A guest paces itself against frames displayed, not produced: its
      // `eglSwapBuffers` waits for this count to move.
      if (this.#glServed) {
        this.#gl?.display?.();
        this.#drawGl();
      }
      this.#reconcile();
    } catch (e) {
      console.error('[dom-desktop]', e);
    }
    this.#raf = requestAnimationFrame(() => this.#frame());
  }

  #reconcile() {
    const { surfaces, blocks } = readScene(this.#channel);
    const live = new Set();
    this.#records = surfaces;
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
      if (this.#cursor === win) {
        // The client took its cursor away: back to the page's own pointer,
        // which is what the scene compositor's fallback arrow amounts to.
        this.#cursor = null;
        this.#cursorShown = -1;
        this.#mount.style.cursor = 'default';
      }
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
    win.placed = !!s.placed;
    win.layer = s.layer || 0;
    win.stackId = s.stackId || 0;
    if ((s.role !== ROLE_TOPLEVEL || s.placed) && (s.relX !== win.relX || s.relY !== win.relY)) {
      win.relX = s.relX;
      win.relY = s.relY;
      // A test's record (tests/eject/w3-page.mjs's drag), beside wl-bridge's.
      globalThis.__gexEvLog?.push(['place', s.relX, s.relY, performance.now()]);
      win.onplace?.(s.relX, s.relY, win.parent);
    }
    // Directly below the placed window with stack id `below`; 0 the top of
    // its layer, 0xffffffff the bottom. The page keeps the order.
    if (s.placed && s.restack !== win.restack) {
      win.restack = s.restack;
      win.onrestack?.(s.below);
    }
    // Its client activated it: it has the keyboard now.
    if (s.placed && s.activate !== win.activate) {
      win.activate = s.activate;
      this.focus(win);
    }
    // Nothing to copy when there is no buffer: a window whose pixels are the
    // host's GL has none, and neither has one the window manager published
    // before the X server had anything to commit. A buffer that *is* there is
    // painted even for an external surface -- the GL frame is drawn over it
    // afterwards (see `#drawGl`).
    if (!s.addr) return;
    if (s.serial === win.serial) return;
    const prev = win.serial;
    win.serial = s.serial;
    const mem = this.#memoryOf(s.token);
    // Damage is a hint about what changed, not a promise that the rest is
    // already right. Paint the whole surface when there is nothing to go on:
    // the first frame, a commit that named no damage at all -- Xwayland does
    // that, which is why xeyes stayed blank -- or a commit this desktop never
    // saw, whose damage it therefore does not have.
    if (mem) {
      win.paint(mem, s, prev < 0 || s.serial !== prev + 1);
    } else if (!this.#silent.has(s.token)) {
      // Said once per process: a window whose memory this desktop was never
      // given is blank for a reason no pixel can show.
      this.#silent.add(s.token);
      console.warn(`[dom-desktop] no memory for token ${s.token}: ${win.title} cannot be painted`);
    }
    if (win === this.#cursor) this.#showCursor(win);
  }

  /** The client's cursor, as this page's cursor.
   *
   * A compositor with a scene draws the cursor sprite into it. This one has no
   * scene, and the browser is already drawing a pointer -- so the client's
   * cursor surface becomes the CSS `cursor` of the desktop, hotspot and all,
   * and the browser puts it where the pointer is. `cursor` is inherited, so
   * one property on the mount covers every window in it.
   *
   * Only when it changes: a `toDataURL` per frame would be absurd for an image
   * that changes when the pointer crosses a text field. */
  #showCursor(win) {
    if (win.serial === this.#cursorShown) return;
    this.#cursorShown = win.serial;
    try {
      const url = win.canvas.toDataURL('image/png');
      const hx = Math.max(0, Math.round(win.relX));
      const hy = Math.max(0, Math.round(win.relY));
      // `default` after it, because a cursor image the browser rejects -- too
      // large, or a hotspot outside it -- must not leave the pointer invisible.
      this.#mount.style.cursor = `url(${url}) ${hx} ${hy}, default`;
    } catch (e) {
      this.#mount.style.cursor = 'default';
    }
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
  /* Draw every window whose pixels are the host's OpenGL.
   *
   * One renderer, several windows: the renderer keeps a frame per surface and
   * puts the one asked for on its own canvas, which is then copied into that
   * window's canvas. One GPU blit and one canvas copy per window per frame,
   * and no readback -- the pixels never come back to JavaScript.
   *
   * The canvas the window adopted used to be the renderer's own, which is one
   * copy fewer and exactly one window: a second GL client had nowhere to be
   * drawn, and closing the first took the renderer's canvas out of the page
   * with the frame it was inside.
   *
   * Which window is which comes from the device rather than from the surface
   * record: a Wayland client names its own surface, an X client names a
   * drawable, and the window manager is what knows which surface that window
   * became. Either way the answer arrives after the window does.
   */
  #drawGl() {
    const device = this.#gl;
    this.#glLooks++;
    if (!device?.ok) {
      // Once, and loudly. A device that dies after it started -- a WebGPU
      // device whose `lost` promise resolves is the way this happens -- fails
      // nothing and says nothing: the client goes on submitting, the frame
      // counters go on climbing, and every GL window is simply black from
      // then on.
      if (device && !this.#glGone) {
        this.#glGone = true;
        const why = device.error || 'no reason given';
        console.error(`[dom-desktop] the GL device is gone: ${why}`);
        this.onglfailed?.(why);
      }
      return;
    }
    // gex_wgpu presents into the canvas it was configured with, so the frame
    // is simply *there* and the copy takes it as it is.
    // Read every frame rather than when a counter says it changed. The
    // counter was an optimisation worth a handful of wasm calls per frame,
    // and it was wrong once -- a list captured while only one client had
    // attached, never refreshed, and a second GL window that was therefore
    // never drawn at all. A window that is missing is much more expensive
    // than sixteen calls.
    this.#glList = device.surfaces();
    if (!this.#glList.length) return;
    for (const [key, win] of this.#windows) {
      const found = this.#glList.find((s) => s.id && (s.kind === 0
        ? win.token === s.token && win.surface === s.id
        : win.xid === s.id));
      if (!found) continue;
      // Its pixels are the host's from now on: nothing paints this window
      // out of the guest's memory any more. See `#update`.
      win.external = true;
      // The frame is already on this desktop's canvas: that is where the
      // backend presents. One canvas, so one frame -- with several GL
      // windows they would all show whichever presented last.
      const serial = device.presentedCount?.() ?? 0;
      if (this.#glShown.get(key) === serial) continue;
      const w = this.#glCanvas.width;
      const h = this.#glCanvas.height;
      if (w < 2 || h < 2) continue;   // still at its placeholder size
      this.#glShown.set(key, serial);
      this.#glCopies++;
      win.showGl(this.#glCanvas, w, h);
    }
    for (const key of [...this.#glShown.keys()])
      if (!this.#windows.has(key)) this.#glShown.delete(key);
  }

  #attach(win) {
    const c = win.canvas;
    // Inside the window, except for a placed window while a button holds the
    // pointer (Wayland's implicit grab): Wine's own move and size loops
    // (SC_MOVE, SC_SIZE) follow the pointer past the window's edge.
    const at = (e) => {
      const x = Math.round(e.offsetX), y = Math.round(e.offsetY);
      if (win.placed && c.hasPointerCapture?.(e.pointerId)) return { x, y, t: Math.round(e.timeStamp) };
      return {
        x: Math.max(0, Math.min(win.width - 1, x)),
        y: Math.max(0, Math.min(win.height - 1, y)),
        t: Math.round(e.timeStamp),
      };
    };
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
      // A pointer that is no longer active makes this throw, and a window
      // that cannot take a drag is better than a listener that dies in it.
      try { c.setPointerCapture?.(e.pointerId); } catch { /* no capture */ }
      sendEvent(this.#channel, win.block, EV_MOTION, p.x, p.y, p.t, 0,
                win.client, win.surface);
      sendEvent(this.#channel, win.block, EV_BUTTON, BTN[e.button] ?? 0x110, 1,
                p.t, 0, win.client, win.surface);
    });
    c.addEventListener('pointerup', (e) => {
      const p = at(e);
      try { c.releasePointerCapture?.(e.pointerId); } catch { /* never held */ }
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
