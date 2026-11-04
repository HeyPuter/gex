// `CompositorWindows`: the guest's whole desktop drawn into one canvas by
// gex's scene compositor (page/src/compositor.js, on WebGPU in a Worker of its
// own): window frames, title bars, stacking, the cursor and its own hit
// testing. For a page that wants a desktop without being a window manager.

import { INTERNAL, internals } from '../internal.js';
import { xOnDemand } from './x11.js';

/** The page's compositor (page/src/compositor.js) for one whose scene is in
 *  the single-thread build's system Worker `k`: what it would post to its
 *  Worker goes as `{ comp }`, what comes back arrives as `comp-out` events
 *  (`listeners`), and its counters, which it cannot share, as copies. */
async function relayed(canvas, scale, k, listeners) {
  // gex-eject's `"compositor": false` builds drop it (runtime.js `openWayland`).
  if (!(typeof __GEX_COMPOSITOR__ === 'undefined' || __GEX_COMPOSITOR__)) throw new Error('this build has no compositor: give `wl` an element (bare-desktop.js)');
  const { Compositor } = await import('../../../src/compositor.js');
  const mine = new Set();
  let stats = null;
  listeners.add((m) => {
    if (m.ev === 'comp-out') for (const f of [...mine]) f({ data: m.m });
    else if (m.ev === 'comp-stats' && stats) stats.set(m.stats.subarray(0, stats.length));
  });
  const port = {
    postMessage(m, transfer = []) {
      if (m?.kind === 'init' && m.stats) stats = new Int32Array(m.stats);
      k.post({ comp: m }, transfer);
    },
    addEventListener(type, f) { if (type === 'message') mine.add(f); },
    removeEventListener(type, f) { mine.delete(f); },
    terminate() {},
  };
  return Compositor.create(canvas, null, canvas, { makeWorker: () => port, scale });
}

export class CompositorWindows extends EventTarget {
  #c = null;

  /**
   * `canvas`: the output, its backing store the output's size in device
   * pixels; `scale`: device pixels per CSS pixel (default devicePixelRatio,
   * rounded), which clients are told. The canvas's control goes to the
   * compositor's Worker. `x11`: 'rootless' (the default), each X window a
   * window of the desktop, placed by the runtime's window manager; or
   * 'rootful', one X screen filling the output, run by the session's own
   * window manager (kwin_x11 and the like).
   */
  constructor(sys, { canvas, scale, x11 = 'rootless' } = {}) {
    super();
    if (typeof canvas?.getContext !== 'function') throw new TypeError('CompositorWindows: a canvas');
    if (x11 !== 'rootless' && x11 !== 'rootful') throw new TypeError("CompositorWindows: x11 is 'rootless' or 'rootful'");
    const i = internals(sys, 'CompositorWindows');
    i.attach('windows', this, { beforeBoot: true });
    i.devices(async (dev) => {
      const wlScale = Math.max(1, Math.round(scale ?? globalThis.devicePixelRatio ?? 1));
      // The single-thread build: the scene is drawn in its system Worker,
      // which holds the memories it reads; this side relays to it.
      if (dev.single) {
        dev.single.wl = {
          width: canvas.width,
          height: canvas.height,
          scale: wlScale,
          attach: async (k, listeners) => ({ compositor: (this.#c = await relayed(canvas, wlScale, k, listeners)) }),
        };
        return;
      }
      Object.assign(dev, await dev.rt.openWayland({ wl: canvas, wlScale, say: (t) => dev.say(`${t}\n`) }));
      this.#c = dev.compositor;
    });
    const x = { rootful: x11 === 'rootful' };
    xOnDemand(i, x);
    this[INTERNAL] = {
      x,
      of: () => [],
      shutdown: () => this.#c?.stop(),
    };
  }

  /** What a program needs to reach this compositor and the X display. */
  get env() {
    return { XDG_RUNTIME_DIR: '/tmp', WAYLAND_DISPLAY: 'wayland-0', DISPLAY: ':0' };
  }

  /** The output's new size, in device pixels. */
  resize(width, height) {
    this.#c?.resize(width, height);
  }

  /** The scene: its windows, titles and a hash of what is shown. */
  describe() {
    return this.#c?.describe() ?? Promise.resolve(null);
  }
}
