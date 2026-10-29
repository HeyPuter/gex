// Input the browser holds back until the page asks: the pointer locked to a
// window (mouse look), the clipboard, and (later) gamepads.

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

const CLIP_POLL_MS = 200;

/**
 * `Clipboard`: the guest's clipboard and the browser's. A program's copy
 * (a Wayland client's selection, or an X program's CLIPBOARD through the X
 * window manager) comes out as a `copy` event (`text`) and,
 * with `sync`, onto the browser's clipboard; the browser's text goes in on
 * `setText`, and with `sync` on a paste into the page and when the page
 * gets the focus (where the browser lets it read the clipboard). Text only.
 * One per system, made any time.
 */
export class Clipboard extends EventTarget {
  #channel = null;
  #seen = 0;
  #timer = 0;
  #sync;
  #off = [];
  #text = '';
  #api = null;

  constructor(sys, { sync = true } = {}) {
    super();
    this.#sync = !!sync;
    const i = internals(sys, 'Clipboard');
    i.attach('clipboard', this);
    i.afterBoot(async (s) => {
      if (!s.wlChannel) {
        i.say('gex: Clipboard: this system has no windows to share a clipboard with\n');
        return;
      }
      this.#api = await import('@gex/wl-bridge.mjs');
      this.#channel = s.wlChannel;
      this.#seen = this.#api.clipboardSerial(this.#channel);
      this.#timer = setInterval(() => this.#poll(), CLIP_POLL_MS);
      if (this.#sync && globalThis.document) {
        const onPaste = (e) => {
          const t = e.clipboardData?.getData('text/plain');
          if (t) this.setText(t);
        };
        const onFocus = () => this.#readHost();
        document.addEventListener('paste', onPaste, true);
        globalThis.addEventListener('focus', onFocus);
        this.#off.push(() => document.removeEventListener('paste', onPaste, true), () => globalThis.removeEventListener('focus', onFocus));
      }
      for (const fn of this.#pending.splice(0)) fn();
    });
  }
  #pending = [];

  /** The clipboard's text as last set, by either side. */
  get text() {
    return this.#text;
  }

  /** Put `text` on the guest's clipboard: every program with a window is
   *  offered it. */
  setText(text) {
    text = String(text);
    this.#text = text;
    if (!this.#channel) {
      this.#pending.push(() => this.setText(text));
      return;
    }
    this.#seen = this.#api.setClipboard(this.#channel, text);
  }

  async #readHost() {
    try {
      const st = await navigator.permissions?.query({ name: 'clipboard-read' });
      if (st && st.state !== 'granted') return;
      const t = await navigator.clipboard.readText();
      if (t && t !== this.#text) this.setText(t);
    } catch {}
  }

  #poll() {
    const serial = this.#api.clipboardSerial(this.#channel);
    if (serial === this.#seen) return;
    this.#seen = serial;
    const c = this.#api.getClipboard(this.#channel);
    if (c.from !== 'guest') return;
    this.#text = c.text;
    this.dispatchEvent(Object.assign(new Event('copy'), { text: c.text }));
    if (this.#sync) navigator.clipboard?.writeText(c.text).catch(() => {});
  }

  /** Stop sharing. */
  close() {
    clearInterval(this.#timer);
    for (const f of this.#off.splice(0)) f();
  }
}
