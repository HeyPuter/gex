// `CompositorWindows`: the guest's whole desktop drawn into one canvas by
// gex's scene compositor (web/src/compositor.js, on WebGPU in a Worker of its
// own): window frames, title bars, stacking, the cursor and its own hit
// testing. For a page that wants a desktop without being a window manager.

import { INTERNAL, internals } from '../internal.js';
import { xOnDemand } from './x11.js';

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
