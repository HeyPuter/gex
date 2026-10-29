// Input the browser holds back until the page asks: the pointer locked to a
// window (mouse look), and later the clipboard and gamepads.

import { internals } from './internal.js';

/**
 * `PointerLock`: a click on a window whose program asks for a locked
 * pointer (Wayland's pointer constraints; Xwayland asks for an X program
 * that grabs the pointer and hides it, as games do) takes the browser's
 * pointer lock, and the mouse's movement reaches the program as relative
 * motion. Escape, or the program letting go, ends it. One per system,
 * made any time; without one no window locks the pointer.
 */
export class PointerLock extends EventTarget {
  #desk = null;
  #locked = false;

  /** Events: `lock` and `unlock`. */
  constructor(sys) {
    super();
    const i = internals(sys, 'PointerLock');
    i.attach('pointer lock', this);
    i.afterBoot((s) => {
      const d = s.compositor;
      if (!d || !('pointerLock' in d)) {
        i.say('gex: PointerLock: this system has no windows to lock the pointer to\n');
        return;
      }
      this.#desk = d;
      d.pointerLock = true;
      d.onpointerlock = (on) => {
        this.#locked = !!on;
        this.dispatchEvent(new Event(on ? 'lock' : 'unlock'));
      };
    });
  }

  /** Whether a window has the pointer now. */
  get locked() {
    return this.#locked;
  }

  /** Let the pointer go (as Escape does). */
  release() {
    if (this.#locked) document.exitPointerLock?.();
  }

  /** No window takes the pointer from now on. */
  close() {
    this.release();
    if (this.#desk) {
      this.#desk.pointerLock = false;
      this.#desk.onpointerlock = null;
    }
  }
}
