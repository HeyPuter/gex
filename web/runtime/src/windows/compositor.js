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
   * compositor's Worker.
   */
  constructor(sys, { canvas, scale } = {}) {
    super();
    if (typeof canvas?.getContext !== 'function') throw new TypeError('CompositorWindows: a canvas');
    const i = internals(sys, 'CompositorWindows');
    i.attach('windows', this, { beforeBoot: true });
    i.beforeBoot((opts) => {
      opts.wl = canvas;
      opts.wlScale = Math.max(1, Math.round(scale ?? globalThis.devicePixelRatio ?? 1));
    });
    i.afterBoot((s) => { this.#c = s.compositor; });
    xOnDemand(i);
    this[INTERNAL] = {
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
