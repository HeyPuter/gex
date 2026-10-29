// `HostWindows`: the guest's windows handed to the page one by one, each a
// bare canvas the page frames and places itself (a desktop's window manager,
// AliceWM in Anura). The guest's compositor is gex's (web/src/dom-compositor.js
// `DomDesktop`): the same protocol in every guest process, the windows drawn
// into their own canvases, input taken from each.

import { INTERNAL, internals } from '../internal.js';
import { xOnDemand } from './x11.js';

const KINDS = { 1: 'toplevel', 2: 'popup', 3: 'subsurface' };
/** `DomDesktop.STATE_*`. */
const STATE = { maximized: 2, resizing: 4, fullscreen: 8 };

/** One guest window, as the page sees it. Events: `title`, `resized`,
 *  `position` (a popup's or a placed window's place changed), `request`
 *  (`kind`: 'move', 'resize' with `edges`, 'maximize', 'unmaximize',
 *  'fullscreen', 'unfullscreen'), `restack` (a placed window's `below`) and
 *  `closed`. */
export class GuestWindow extends EventTarget {
  #w;
  #d;
  #host;
  #states = { maximized: false, fullscreen: false, resizing: false };

  constructor(host, desktop, w) {
    super();
    this.#host = host;
    this.#d = desktop;
    this.#w = w;
    /** The window's pixels. The page puts it where it likes; its CSS size
     *  is set here (`width`, `height`) and follows the client's. */
    this.canvas = w.canvas;
    /** 'toplevel', 'popup' or 'subsurface'. */
    this.kind = KINDS[w.role] ?? 'toplevel';
    const c = this.canvas;
    c.tabIndex = 0;
    c.style.outline = 'none';
    c.dataset.gexWindow = w.key;
    this.#size();
    // Keys go to the window whose canvas has the page's focus.
    c.addEventListener('keydown', (e) => this.#host.keyEvent(e, true));
    c.addEventListener('keyup', (e) => this.#host.keyEvent(e, false));
    c.addEventListener('focus', () => desktop.focus(w));
    w.onretitle = () => this.dispatchEvent(new Event('title'));
    w.onresized = () => {
      this.#size();
      this.dispatchEvent(new Event('resized'));
    };
    w.onplace = () => this.dispatchEvent(new Event('position'));
    w.onrestack = (below) => this.dispatchEvent(Object.assign(new Event('restack'), { below }));
    w.onrequest = (kind, edges) => this.dispatchEvent(Object.assign(new Event('request'), { kind, edges }));
    w.onfullscreen = (on) => this.dispatchEvent(Object.assign(new Event('request'), { kind: on ? 'fullscreen' : 'unfullscreen' }));
  }

  #size() {
    this.canvas.style.width = `${this.width}px`;
    this.canvas.style.height = `${this.height}px`;
  }

  /** The pid of the process that made it (an X window's is the X server's). */
  get pid() {
    return this.#w.token;
  }

  /** The window it belongs to (a popup's or a subsurface's), or null. */
  get parent() {
    const w = this.#w;
    if (!w.parent || this.kind === 'toplevel') return null;
    return this.#host[INTERNAL].byKey(`${w.token}:${w.client}:${w.parent}`);
  }

  get title() {
    return this.#w.title ?? '';
  }

  /** Not known yet: the guest's compositor does not pass it on. */
  get appId() {
    return null;
  }

  /** Who draws its frame: 'host' (the page), 'client' (the program: a
   *  Win32 window under Wine, a GTK 4 window), or 'none' (a popup, a
   *  subsurface: never framed). */
  get decorations() {
    if (this.kind !== 'toplevel') return 'none';
    return this.#w.placed ? 'client' : 'host';
  }

  /** Placed by its client (a Win32 window under Wine, an X menu): `x`, `y`
   *  are where it goes on the output, and its client moves, sizes and
   *  stacks it (`restack`); the page never does. */
  get placed() {
    return !!this.#w.placed;
  }

  /** Where its client puts it: a popup or a subsurface relative to its
   *  parent's canvas, a placed window on the output. 0 for a toplevel the
   *  page places. */
  get x() {
    return this.kind === 'toplevel' && !this.placed ? 0 : this.#w.relX | 0;
  }

  get y() {
    return this.kind === 'toplevel' && !this.placed ? 0 : this.#w.relY | 0;
  }

  /** Its size in CSS pixels. */
  get width() {
    return Math.round(this.#w.width / (this.#w.bufferScale || 1));
  }

  get height() {
    return Math.round(this.#w.height / (this.#w.bufferScale || 1));
  }

  /** What the page last said of it (`setStates`). */
  get states() {
    return { ...this.#states };
  }

  /** Ask the client to be this size (CSS pixels); it answers by drawing at
   *  it (`resized`). */
  resize(w, h) {
    this.#w.resize(w, h);
  }

  /** Tell the client it is maximized, fullscreen or being resized, at a
   *  size (default its own). */
  setStates({ maximized, fullscreen, resizing } = {}, w, h) {
    if (maximized !== undefined) this.#states.maximized = !!maximized;
    if (fullscreen !== undefined) this.#states.fullscreen = !!fullscreen;
    if (resizing !== undefined) this.#states.resizing = !!resizing;
    let bits = 0;
    for (const [k, b] of Object.entries(STATE)) if (this.#states[k]) bits |= b;
    this.#w.setStates(bits, w ?? this.width, h ?? this.height);
  }

  /** Where the page put it on the output, in CSS pixels: an X window's
   *  menus and drags read it; a placed window's pointer is relative to it.
   *  Call it after each move. */
  place(x, y) {
    this.#w.place(x, y);
  }

  /** Give it the keyboard (and the page's focus). */
  focus() {
    this.#d.focus(this.#w);
    if (globalThis.document?.activeElement !== this.canvas) this.canvas.focus({ preventScroll: true });
  }

  /** Ask it to close, as a frame's close button does. */
  close() {
    this.#w.close();
  }
}

export class HostWindows extends EventTarget {
  #i;
  #d = null;
  #mount = null;
  #size;
  #wins = new Map();

  /**
   * The guest's windows, each handed to the page (`window` event) to frame
   * and place. `width`, `height`: the output's size in CSS pixels, what a
   * program asking for the screen's size sees (default the viewport);
   * `scale`: device pixels per CSS pixel (default devicePixelRatio, rounded).
   * X11 programs' windows come as any other, through the runtime's X server,
   * started for the first of them.
   */
  constructor(sys, { width, height, scale } = {}) {
    super();
    const i = internals(sys, 'HostWindows');
    i.attach('windows', this, { beforeBoot: true });
    this.#i = i;
    this.#size = this.#fit(width, height, scale);
    xOnDemand(i);
    i.beforeBoot((opts) => {
      // DomDesktop's element: where it would listen for keys and set the
      // cursor. Out of sight: the windows are the page's, wherever it puts
      // them, and keys come from their canvases.
      const m = document.createElement('div');
      m.style.cssText = 'position:fixed;left:0;top:0;width:0;height:0;overflow:hidden;';
      m.dataset.gexDesktop = '';
      document.body.append(m);
      this.#mount = m;
      opts.wl = m;
      opts.onDesktop = (d) => this.#start(d);
    });
    const self = this;
    this[INTERNAL] = {
      byKey: (k) => self.#wins.get(k) ?? null,
      of: (pid) => [...self.#wins.values()].filter((w) => w.pid === pid),
      shutdown: () => {
        self.#d?.stop();
        self.#mount?.remove();
      },
    };
  }

  #fit(width, height, scale) {
    const s = Math.max(1, Math.round(scale ?? globalThis.devicePixelRatio ?? 1));
    return {
      w: Math.max(1, Math.round(width ?? globalThis.innerWidth ?? 1280)),
      h: Math.max(1, Math.round(height ?? globalThis.innerHeight ?? 720)),
      scale: s,
    };
  }

  /** The environment a program needs to reach this compositor and the X
   *  display: add it to `spawn`'s `env`. */
  get env() {
    return { XDG_RUNTIME_DIR: '/tmp', WAYLAND_DISPLAY: 'wayland-0', DISPLAY: ':0' };
  }

  /** The output's new size (CSS pixels) and scale, when the page's desktop
   *  changes: what programs see as their screen from now on. */
  setOutputSize(width, height, scale) {
    this.#size = this.#fit(width, height, scale ?? this.#size.scale);
    this.#d?.resizeOutput(this.#size.w * this.#size.scale, this.#size.h * this.#size.scale, this.#size.scale);
  }

  /** Every window there is, oldest first. */
  windows() {
    return [...this.#wins.values()];
  }

  /** The window with the keyboard, or null. */
  get focused() {
    const w = this.#d?.focused();
    return w ? this.#wins.get(w.key) ?? null : null;
  }

  /** A key the page heard somewhere other than a window's canvas (its
   *  frame, say), for the window with the keyboard. */
  keyEvent(e, pressed) {
    this.#d?.keyEvent(e, pressed);
  }

  #start(d) {
    this.#d = d;
    const { w, h, scale } = this.#size;
    d.resizeOutput(w * scale, h * scale, scale);
    // The client's cursor, on every window's canvas (a `cursor` event says
    // it too, for a page that draws its own).
    d.oncursor = (css) => {
      for (const win of this.#wins.values()) win.canvas.style.cursor = css;
      this.dispatchEvent(Object.assign(new Event('cursor'), { css }));
    };
    d.onwindow = (dw) => {
      const win = new GuestWindow(this, d, dw);
      this.#wins.set(dw.key, win);
      dw.onclosed = () => {
        this.#wins.delete(dw.key);
        win.canvas.remove();
        win.dispatchEvent(new Event('closed'));
      };
      this.dispatchEvent(Object.assign(new Event('window'), { window: win }));
    };
  }
}
