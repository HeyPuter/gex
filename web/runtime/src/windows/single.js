// `SingleCanvas`: the guest's windows drawn straight into one element of the
// page, laid out there with no frames (web/src/bare-desktop.js): for a page
// that is one program -- a game, a Win32 program whose windows draw their own
// frames -- rather than a desktop.

import { INTERNAL, internals } from '../internal.js';
import { xOnDemand } from './x11.js';

export class SingleCanvas extends EventTarget {
  #d = null;
  #container;

  /**
   * `container`: a positioned element; its size is the output's, what a
   * program asking for the screen's size sees. The windows' canvases go in
   * it, the first at its corner, popups over their parents, a window its
   * client places (Wine's) where it says.
   */
  constructor(sys, { container } = {}) {
    super();
    if (!container || container.nodeType !== 1) throw new TypeError('SingleCanvas: a container element');
    const i = internals(sys, 'SingleCanvas');
    this.#container = container;
    i.attach('windows', this, { beforeBoot: true });
    i.devices(async (dev) => {
      const { bareDesktop } = await import('../../../src/bare-desktop.js');
      const onDesktop = (d) => {
        this.#d = bareDesktop(d, container);
        d.onwindowgone = () => this.dispatchEvent(new Event('change'));
        const onwindow = d.onwindow;
        d.onwindow = (w) => {
          onwindow(w);
          this.dispatchEvent(new Event('change'));
        };
      };
      Object.assign(dev, await dev.rt.openWayland({ wl: container, onDesktop, say: (t) => dev.say(`${t}\n`) }));
    });
    xOnDemand(i);
    this[INTERNAL] = {
      of: () => [],
      shutdown: () => this.#d?.stop(),
    };
  }

  /** What a program needs to reach this compositor and the X display. */
  get env() {
    return { XDG_RUNTIME_DIR: '/tmp', WAYLAND_DISPLAY: 'wayland-0', DISPLAY: ':0' };
  }

  /** The container's new size, as the output's: after the page resized it. */
  resized() {
    this.#d?.resizeOutput(this.#container.clientWidth, this.#container.clientHeight);
  }

  /** The windows' titles, surfaces and a hash of what is shown, as a test
   *  harness reads it (`change` events say when it changes). */
  describe() {
    return this.#d?.describe?.() ?? null;
  }
}
