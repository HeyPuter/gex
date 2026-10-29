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
// address to copy from: its frames are a texture in the renderer that runs
// the guest's GL. Such a window gets a WebGPU canvas of its own, laid over its
// 2D one, and the renderer presents into it -- see `#drawGl` below.

import {
  readScene,
  sceneMarks,
  sceneMark,
  sceneStill,
  ackBlock,
  releaseToken,
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
  F_FULLSCREEN,
  ROLE_TOPLEVEL,
  ROLE_POPUP,
  ROLE_SUBSURFACE,
  ROLE_CURSOR,
  takesPointer,
} from '@gex/wl-bridge.mjs';
import { pageParams } from './page-env.js';
import { EVDEV, BTN, BIT, modsOf } from './input-map.js';
import { logNamed } from '@gex/log.mjs';

/** `GEX_LOG=win` (only when named): a line when a toplevel appears, first
 *  shows pixels (its first buffer painted, or its GL frame shown) and is
 *  retitled, for a harness timing a window's arrival. Nothing per frame. */
function winNote(win, what) {
  if (win.role === ROLE_TOPLEVEL && logNamed('win')) console.log(`[win] ${what} ${win.key} ${win.width}x${win.height} "${win.title ?? ''}"`);
}

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
    /** The host GL's own canvas for this window, laid over `canvas`, and the
     * renderer's surface uid it shows (`frame` in the device's list); see
     * `showGlCanvas`. */
    this.glCanvas = null;
    this.glFrame = 0;
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
    /** The client's window-management requests from its own frame:
     *  `onrequest(kind, edges)`, kind 'move', 'resize' (`edges`: 1 top,
     *  2 bottom, 4 left, 8 right), 'maximize' or 'unmaximize'; each once. */
    this.onrequest = null;
    this.requestSeq = undefined;
    /** `onfullscreen(on)`: the client asked to be fullscreen, or not. */
    this.onfullscreen = null;
    this.fullscreen = false;
    /** The states the page has given it (`setStates`): STATE_MAXIMIZED,
     *  STATE_FULLSCREEN, STATE_RESIZING; activation is the keyboard's. */
    this.states = 0;
    /** Buffer pixels per surface unit (`wl_surface.set_buffer_scale`). */
    this.bufferScale = 1;
    this.#desktop = desktop;
  }

  #desktop;
  #ctx = null;
  #image = null;
  #warned = false;

  /** Where the page has put this window, in output coordinates.
   *
   * A no-op for a Wayland toplevel and required for an X one: see the note at
   * the top. Call it after moving the element, including while dragging. For
   * a placed window (`gex_window_placement_v1`) it is the origin its client
   * adds to the surface-local pointer positions sent after it; call it when
   * the element moves to where the client asked (`onplace`). */
  place(x, y) {
    this.x = Math.round(x);
    this.y = Math.round(y);
    this.#desktop.moved(this);
  }

  /** Ask the client to be this size. It answers by drawing at it. */
  resize(w, h) {
    this.#desktop.configure(this, Math.max(1, Math.round(w)), Math.max(1, Math.round(h)));
  }

  /** Tell the client its states (`DomDesktop.STATE_*`), with its size. */
  setStates(states, w = this.width, h = this.height) {
    this.states = states & ~1;
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

  /** Give this window a WebGPU canvas of its own for surface `uid`, which the
   * renderer presents into directly (`#gex-gl-<uid>`).
   *
   * Laid over `canvas` as its next sibling, the same size, and transparent to
   * the pointer, so input still lands on `canvas` with the same offsets. Not
   * `canvas` itself: that one may already have its 2D context -- a Wine
   * window is painted from memory before its GL starts -- and a canvas keeps
   * the first context it was given.
   *
   * This used to be a 2D `drawImage` of the renderer's shared canvas into
   * `canvas`. On Chromium on Linux that read of a WebGPU canvas came back
   * fully transparent (a red clear too), so a GL window counted every frame
   * presented and showed nothing. Presenting into the window's own canvas
   * keeps the frame on the GPU end to end, with no copy through 2D at all.
   *
   * False while `canvas` is not in the document: the renderer finds the
   * canvas by its id, and one outside the document is not found. */
  showGlCanvas(uid) {
    if (!this.canvas.isConnected) return false;
    let c = this.glCanvas;
    if (!c) {
      c = document.createElement('canvas');
      c.style.position = 'absolute';
      c.style.pointerEvents = 'none';
      c.style.touchAction = 'none';
      this.glCanvas = c;
    }
    c.id = `gex-gl-${uid}`;
    this.glFrame = uid;
    this.placeGl();
    return true;
  }

  /** Keep the GL canvas over `canvas`, wherever the page has put that. */
  placeGl() {
    const c = this.glCanvas;
    if (!c) return;
    const under = this.canvas;
    if (c.previousSibling !== under && under.isConnected) under.after(c);
    const hidden = !under.isConnected || under.style.display === 'none' ||
      under.style.visibility === 'hidden';
    c.style.display = hidden ? 'none' : 'block';
    if (hidden) return;
    // The same stacking as the window: a page that stacks its windows by
    // `z-index` (the 32-bit Wine shell does) would otherwise put this canvas under
    // the very window it belongs to. Equal z-index and later in the
    // document is above it.
    const z = under.style.zIndex || getComputedStyle(under).zIndex;
    const zi = z === 'auto' ? '' : z;
    if (c.style.zIndex !== zi) c.style.zIndex = zi;
    const left = `${under.offsetLeft}px`, top = `${under.offsetTop}px`;
    const width = `${under.offsetWidth}px`, height = `${under.offsetHeight}px`;
    if (c.style.left !== left) c.style.left = left;
    if (c.style.top !== top) c.style.top = top;
    if (c.style.width !== width) c.style.width = width;
    if (c.style.height !== height) c.style.height = height;
  }

  /** The window is no longer the host's GL, or is gone. */
  dropGl() {
    this.glCanvas?.remove();
    this.glCanvas = null;
    this.glFrame = 0;
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
    if (this.paints++ === 0) winNote(this, 'painted');
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
  /** A configure's states (`gex_wayland::STATE_*`). */
  static STATE_MAXIMIZED = 2;
  static STATE_RESIZING = 4;
  static STATE_FULLSCREEN = 8;
  #channel;
  #mount;
  #windows = new Map();
  #memories = new Map();
  #focus = null;
  /** The windows that had the keyboard before, newest last: where it goes
   *  back to when the window with it goes away. */
  #focusPrev = [];
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
  #glList = [];
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
   *  for the screen's size sees this. Windows already up keep their places.
   *  `scale`, when given: device pixels per surface unit (wl_output.scale),
   *  which clients that connect after it are told. */
  resizeOutput(w, h, scale) {
    this.#touched++;
    setOutputSize(this.#channel, Math.max(1, Math.round(w)), Math.max(1, Math.round(h)), scale);
  }

  /** Run the guest's OpenGL for this desktop.
   *
   * The device is built here and now, before any client has drawn: the guest's
   * first GL call is an ioctl that waits for an answer, and a device built
   * later than that is a thirty-second stall and a trap.
   *
   * The device is given a canvas of this desktop's own, which is never
   * shown: the renderer needs one to start, and presents there only a
   * surface no window has claimed yet. Each window that is the host's GL
   * gets a canvas of its own, presented into directly (see `#drawGl`).
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
      source: opts.source,
      api: opts.api || 'core',
      // `?mesaenv=K=V,K=V` and `?wgslcache=`, as compositor.js passes them to
      // the GL worker: without them this path never saw Mesa's switches.
      env: pageParams().get('mesaenv') ?? undefined,
      wgslCache: pageParams().get('wgslcache') ?? undefined,
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
    const { serveGl } = await import('@gex/gl-bridge.mjs');
    this.#glServe = serveGl(device, channel);
    this.#glServed = true;
    this.ongl?.(device.describe());
    return device.describe();
  }

  /** Told when the guest's OpenGL is up, or would not start. */
  ongl = null;
  onglfailed = null;

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
    this.#touched++;
    setKernelMemory(this.#channel, kernel);
    // With pixel buffers in the channel the memory is neither sent nor read
    // (wl-bridge.mjs); without them, as before.
    if (memory && !this.#channel.pixels) this.#memories.set(token, { memory, view: new Uint8Array(memory.buffer) });
  }

  /** A guest's new pixel buffer (wl-bridge.mjs `WlProxy`), `id` in its
   *  surfaces' records; the one it `replaces` is no longer read. */
  addPixels(token, id, buffer, replaces) {
    this.#touched++;
    if (replaces) this.#pixelBufs.delete(replaces);
    if (id && buffer) this.#pixelBufs.set(id, { token, view: new Uint8Array(buffer) });
  }

  /** The bytes a surface's address points into: the pixel buffer its record
   *  names, or for a channel without them, its process's memory. Null until
   *  that buffer has come. */
  #pixelsOf(s) {
    if (!this.#channel.pixels) return this.#memoryOf(s.token);
    return this.#pixelBufs.get(s.pix)?.view ?? null;
  }
  #pixelBufs = new Map();

  /** The current view of a process's memory, re-derived if it has grown. */
  #memoryOf(token) {
    const held = this.#memories.get(token);
    if (!held) return null;
    // A shared memory grown by another thread can keep its buffer object and
    // only grow its length (overlay-worker.js's check): a view from before
    // then stops short, and the windows of a process that grew past it (a
    // desktop and taskbar, with explorer /desktop a process of its own)
    // were never painted again.
    const buf = held.memory.buffer;
    if (held.view.buffer !== buf || held.view.length !== buf.byteLength) {
      held.view = new Uint8Array(buf);
    }
    return held.view;
  }

  dropMemory(token) {
    this.#touched++;
    this.#memories.delete(token);
    for (const [id, p] of this.#pixelBufs) if (p.token === token) this.#pixelBufs.delete(id);
    // Its block in the arena too, as compositor-worker.js does: a process that
    // exited kept its block here, so on a page with more than a few window
    // processes a new one took a live, idle process's block (its heartbeat is
    // its last publish) -- a desktop's windows were then read out of
    // another process's memory and never painted.
    releaseToken(this.#channel, token);
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
    for (const win of this.#windows.values()) win.dropGl();
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

  /** The window at a point of the page (`clientX`, `clientY`: viewport CSS
   *  pixels): {key, client, block, surface, title, x, y}, the point in its
   *  surface in the units its pointer motion has (surface-local, as a drag's
   *  wl_data_device.motion will be) and the title of the toplevel it is
   *  part of; or null. As the compositor's `windowAt`; the browser's hit
   *  test, as the pointer's. */
  windowAt(clientX, clientY) {
    const wins = [...this.#windows.values()];
    for (const el of document.elementsFromPoint(clientX, clientY)) {
      const win = wins.find((w) => w.canvas === el || w.glCanvas === el);
      if (!win) continue;
      let top = win;
      for (let i = 0; top.parent && i < 16; i++) {
        const up = wins.find((w) => w.token === top.token && w.client === top.client && w.surface === top.parent);
        if (!up) break;
        top = up;
      }
      const r = el.getBoundingClientRect();
      return {
        key: win.key,
        client: win.client,
        block: win.block,
        surface: win.surface,
        title: top.title || '',
        // As the pointer's motion has them (#attach: offsetX, offsetY).
        x: Math.round(clientX - r.left),
        y: Math.round(clientY - r.top),
      };
    }
    return null;
  }

  /** The window keys go to (null: none), for a harness. */
  focused() {
    return this.#focus;
  }

  // -- the frame ----------------------------------------------------------

  #frame() {
    if (!this.#running) return;
    try {
      // A guest paces itself against frames displayed, not produced: its
      // `eglSwapBuffers` waits for this count to move.
      // Windows are paired with their GL canvases first, so a frame that
      // arrives in the same page frame as its window goes straight there.
      if (this.#glServed) {
        this.#drawGl();
        this.#gl?.display?.();
      }
      if (!this.#still()) this.#reconcile();
    } catch (e) {
      console.error('[dom-desktop]', e);
    }
    this.#raf = requestAnimationFrame(() => this.#frame());
  }

  // An idle frame (`#still`), as the scene compositor's: what the last
  // reconcile drew from -- each block's token and generation, and the
  // page's changes (`#touched`) by then.
  #marks = sceneMarks();
  #touched = 1;
  #markedTouch = 0;
  /** A window the last reconcile could not paint yet (its pixels had not
   *  come): the next frame is not an idle one. */
  #unpainted = false;

  /** Whether this frame would reconcile what the last one did: no block
   *  claimed, freed or published to, every publish acked, nothing from the
   *  page and every window painted. Then it reads nothing and allocates
   *  nothing. */
  #still() {
    return this.#touched === this.#markedTouch && !this.#unpainted && sceneStill(this.#channel, this.#marks);
  }

  #reconcile() {
    this.#unpainted = false;
    const touched = this.#touched;
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
        winNote(win, 'new');
        // Not a placed one: its client says when it has the keyboard
        // (`activate`), as in the scene compositor. A Start menu may show
        // the user's picture as a placed toplevel of its own; it took the
        // keyboard on appearing, search hid it, and with the focus gone every
        // key after the first was dropped.
        if (s.role === ROLE_TOPLEVEL && !s.placed) this.focus(win);
      }
      this.#update(win, s);
    }
    for (const [k, win] of [...this.#windows]) {
      if (live.has(k)) continue;
      this.#windows.delete(k);
      this.#focusPrev = this.#focusPrev.filter((w) => w !== win);
      if (this.#focus === win) {
        // Back to the window that had it before, or the client's next key goes
        // nowhere: Win32 does not activate anything when a menu, a tooltip or
        // the Start menu's user picture goes, and winewayland takes the enter
        // of a placed window as where keys go, not as an activation.
        this.#focus = null;
        const prev = [...this.#focusPrev].reverse().find((w) => this.#windows.get(w.key) === w);
        if (prev) this.focus(prev);
      }
      if (this.#cursor === win) {
        // The client took its cursor away: back to the page's own pointer,
        // which is what the scene compositor's fallback arrow amounts to.
        this.#cursor = null;
        this.#cursorShown = -1;
        this.#setCursor('default');
      }
      if (win.glFrame) this.#gl?.surfaceCanvas?.(win.glFrame, false);
      win.dropGl();
      win.onclosed?.();
      this.onwindowgone?.(win);
    }
    for (const b of blocks) ackBlock(this.#channel, b.block, b.gen);
    sceneMark(this.#marks, blocks);
    this.#markedTouch = touched;
  }

  #update(win, s) {
    if (s.title !== win.title) {
      win.title = s.title;
      win.onretitle?.(s.title);
      winNote(win, 'title');
    }
    if (s.width !== win.width || s.height !== win.height) {
      win.width = s.width;
      win.height = s.height;
      win.canvas.width = Math.max(1, s.width);
      win.canvas.height = Math.max(1, s.height);
      win.onresized?.(s.width, s.height);
    }
    // A surface whose input region is empty takes no pointer: the pointer
    // goes to what is under it, as in the scene compositor's hit test. A
    // client's subsurface for a window's GL picture is one, laid over its
    // parent, and its parent must get the pointer. A region that is not
    // empty is taken as the whole surface here.
    win.bufferScale = (s.flags >> 8) & 0xff || 1;
    // A client's request from its own frame (a GTK title bar, a client-side
    // border), acted on once per number; the first sight of a window only
    // learns the number. As the scene compositor reads it.
    const seq = (s.flags >> 3) & 0xf;
    if (win.requestSeq === undefined) win.requestSeq = seq;
    else if (seq !== win.requestSeq && s.role === ROLE_TOPLEVEL && !s.placed) {
      win.requestSeq = seq;
      const code = (s.flags >>> 24) & 0x7f;
      const kind = ['none', 'move', 'resize', 'maximize', 'unmaximize'][code & 7];
      if (kind && kind !== 'none') win.onrequest?.(kind, (code >> 3) & 0xf);
    }
    const fs = (s.flags & F_FULLSCREEN) !== 0 && s.role === ROLE_TOPLEVEL && !s.placed;
    if (fs !== win.fullscreen) {
      win.fullscreen = fs;
      win.onfullscreen?.(fs);
    }
    const noInput = !takesPointer(s);
    if (noInput !== win.noInput) {
      win.noInput = noInput;
      win.canvas.style.pointerEvents = noInput ? 'none' : '';
    }
    // A popup, a subsurface or an X menu is placed by its client, not by the
    // page: say where it asked to be.
    win.placed = !!s.placed;
    win.layer = s.layer || 0;
    win.stackId = s.stackId || 0;
    if ((s.role !== ROLE_TOPLEVEL || s.placed) && (s.relX !== win.relX || s.relY !== win.relY)) {
      win.relX = s.relX;
      win.relY = s.relY;
      // A test's record (tests/eject/wine32-page.mjs's drag), beside wl-bridge's.
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
    const mem = this.#pixelsOf(s);
    // A commit is only taken once it is painted: a process's memory can reach
    // the page after its first publish, and an idle client commits no more,
    // so a commit taken without the memory would leave its window blank.
    if (mem) win.serial = s.serial;
    else this.#unpainted = true;
    // Damage is a hint about what changed, not a promise that the rest is
    // already right. Paint the whole surface when there is nothing to go on:
    // the first frame, a commit that named no damage at all -- Xwayland does
    // that, which is why xeyes stayed blank -- or a commit this desktop never
    // saw, whose damage it therefore does not have.
    if (mem) {
      win.paint(mem, s, prev < 0 || s.serial !== prev + 1);
    } else if (!this.#channel.pixels && !this.#silent.has(s.token)) {
      // (A pixel buffer on its way is not this: it is painted when it comes.)
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
      this.#setCursor(`url(${url}) ${hx} ${hy}, default`);
    } catch (e) {
      this.#setCursor('default');
    }
  }

  /** The client's cursor as a CSS `cursor` value: on the mount, which every
   *  window in it inherits, or to `oncursor(css)` when the page puts the
   *  windows elsewhere. */
  #setCursor(css) {
    if (this.oncursor) this.oncursor(css);
    else this.#mount.style.cursor = css;
  }

  oncursor = null;

  // -- what the page asks for ---------------------------------------------

  moved(win) {
    sendEvent(this.#channel, win.block, EV_MOVED, win.x, win.y, 0, 0,
              win.client, win.surface);
  }

  configure(win, w, h) {
    sendEvent(this.#channel, win.block, EV_CONFIGURE, w, h, win.states | 0,
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
    this.#focusPrev = this.#focusPrev.filter((w) => w !== win && w !== old);
    if (old) this.#focusPrev.push(old);
    if (this.#focusPrev.length > 16) this.#focusPrev.shift();
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
  /* Pair every window whose pixels are the host's OpenGL with its frame.
   *
   * One renderer, several windows: each window gets a WebGPU canvas of its
   * own (`DomWindow.showGlCanvas`) and the renderer presents that window's
   * surface into it, texture to texture. Nothing is copied here and nothing
   * comes back to JavaScript.
   *
   * Which window is which comes from the device rather than from the surface
   * record: a Wayland client names its own surface, an X client names a
   * drawable, and the window manager is what knows which surface that window
   * became. Either way the answer arrives after the window does.
   */
  #drawGl() {
    const device = this.#gl;
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
    // Read every frame rather than when a counter says it changed. The
    // counter was an optimisation worth a handful of wasm calls per frame,
    // and it was wrong once -- a list captured while only one client had
    // attached, never refreshed, and a second GL window that was therefore
    // never drawn at all. A window that is missing is much more expensive
    // than sixteen calls.
    this.#glList = device.surfaces();
    for (const win of this.#windows.values()) {
      const found = this.#glList.find((s) => s.id && (s.kind === 0
        ? win.token === s.token && win.surface === s.id
        : win.xid === s.id));
      if (!found) {
        // Its surface went away (the client recreated it, or exited): the
        // window is painted from memory again until a new one turns up.
        if (win.glFrame) {
          device.surfaceCanvas?.(win.glFrame, false);
          win.dropGl();
        }
        continue;
      }
      // Its pixels are the host's from now on: nothing paints this window
      // out of the guest's memory any more. See `#update`.
      win.external = true;
      if (win.glFrame !== found.frame) {
        if (win.glFrame) device.surfaceCanvas?.(win.glFrame, false);
        if (!win.showGlCanvas(found.frame)) continue;
        device.surfaceCanvas?.(found.frame, true);
        if (!win.glShown) { win.glShown = true; winNote(win, 'gl'); }
      }
      win.placeGl();
    }
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
    // The buttons this window has been told are down. A pointer's first
    // button is its `pointerdown` and its last one's release its `pointerup`;
    // a button pressed or released while another is held (a right click
    // during a left drag) is neither, only a `pointermove` that names it
    // (`button` >= 0, `buttons` the state after).
    const held = new Set();
    const button = (e, pressed, p) => {
      const code = BTN[e.button];
      if (code === undefined || held.has(code) === pressed) return;
      if (pressed) held.add(code);
      else held.delete(code);
      sendEvent(this.#channel, win.block, EV_BUTTON, code, pressed ? 1 : 0,
                p.t, 0, win.client, win.surface);
    };
    c.addEventListener('pointermove', (e) => {
      const p = at(e);
      sendEvent(this.#channel, win.block, EV_MOTION, p.x, p.y, p.t, 0,
                win.client, win.surface);
      if (e.button >= 0) button(e, (e.buttons & (BIT[e.button] ?? 0)) !== 0, p);
    });
    c.addEventListener('pointerdown', (e) => {
      const p = at(e);
      this.focus(win);
      // A pointer that is no longer active makes this throw, and a window
      // that cannot take a drag is better than a listener that dies in it.
      try { c.setPointerCapture?.(e.pointerId); } catch { /* no capture */ }
      sendEvent(this.#channel, win.block, EV_MOTION, p.x, p.y, p.t, 0,
                win.client, win.surface);
      button(e, true, p);
    });
    c.addEventListener('pointerup', (e) => {
      const p = at(e);
      try { c.releasePointerCapture?.(e.pointerId); } catch { /* never held */ }
      button(e, false, p);
    });
    // The browser took the pointer (a touch turned into a scroll, the page
    // lost it): whatever is down is up, or the client keeps a drag going.
    c.addEventListener('pointercancel', (e) => {
      const t = Math.round(e.timeStamp);
      for (const code of held) sendEvent(this.#channel, win.block, EV_BUTTON, code, 0, t, 0, win.client, win.surface);
      held.clear();
    });
    c.addEventListener('wheel', (e) => {
      e.preventDefault();
      // Wayland's axis is a fixed-point number of surface units; the browser's
      // is pixels, lines or pages. The compositor makes wheel notches of it at
      // 100 px a notch (gex-wayland WHEEL_PX, Chromium's deltaY for one), so a
      // line is a third of that (a notch scrolls three) and a page is the
      // window. A trackpad's many small pixel deltas add up there.
      const scale = e.deltaMode === 1 ? 100 / 3 : e.deltaMode === 2 ? (win.height || 600) : 1;
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
    const down = (e) => this.keyEvent(e, true);
    const up = (e) => this.keyEvent(e, false);
    // The mouse's buttons are the guest's, not the browser's: no context
    // menu over the desktop (it opens on the right button's press on Linux
    // and macOS, and takes the release with it: the client never sees the
    // button come up), no autoscroll on the middle one, no history step on
    // back and forward. The pointer events above still go to the client.
    const ctx = (e) => e.preventDefault();
    const mouse = (e) => { if (e.button === 1 || e.button === 3 || e.button === 4) e.preventDefault(); };
    this.#mount.addEventListener('keydown', down);
    this.#mount.addEventListener('keyup', up);
    this.#mount.addEventListener('contextmenu', ctx);
    this.#mount.addEventListener('mousedown', mouse);
    this.#mount.addEventListener('mouseup', mouse);
    this.#mount.addEventListener('auxclick', mouse);
    // A desktop takes the keyboard, so it has to be focusable.
    if (!this.#mount.hasAttribute('tabindex')) this.#mount.tabIndex = 0;
    this.#detach = () => {
      this.#mount.removeEventListener('keydown', down);
      this.#mount.removeEventListener('keyup', up);
      this.#mount.removeEventListener('contextmenu', ctx);
      this.#mount.removeEventListener('mousedown', mouse);
      this.#mount.removeEventListener('mouseup', mouse);
      this.#mount.removeEventListener('auxclick', mouse);
    };
  }

  #detach = null;

  /** A `keydown` (`pressed`) or `keyup` the page heard, to the window with
   *  the keyboard: for a page that listens for keys somewhere other than
   *  the mount. */
  keyEvent(e, pressed) {
    const win = this.#focus;
    if (!win) return;
    const code = EVDEV[e.code];
    if (code === undefined) return;
    e.preventDefault();
    sendEvent(this.#channel, win.block, EV_MODS, modsOf(e), 0, 0, 0, win.client, 0);
    sendEvent(this.#channel, win.block, EV_KEY, code, pressed ? 1 : 0,
              Math.round(e.timeStamp), 0, win.client, win.surface);
  }

  /** The cursor surface a client asked for, if it has one: a page that wants
   *  to draw it puts this canvas at the pointer, offset by its hotspot. */
  cursor() {
    return this.#cursor;
  }
}

export { ROLE_TOPLEVEL, ROLE_POPUP, ROLE_SUBSURFACE, ROLE_CURSOR };
