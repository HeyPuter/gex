// Input the browser holds back until the page asks: the pointer locked to a
// window (mouse look), the clipboard, and game controllers.

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

// The table's layout: hosts/kernel/src/joy.rs, and the pad words
// gex-kernel's vfs/evdev.rs reads.
const J_HEADER = 16;
const J_CHANGED = 4;
const P_SERIAL = 0, P_GEN = 1, P_KIND = 2, P_ID = 3, P_COUNTS = 4, P_PRESSED = 5;
const P_VALUES = 6, P_AXES = 14, P_NAME = 30, P_TIME = 62;
const MAX_BUTTONS = 32, MAX_AXES = 16, NAME_BYTES = 128;

/** A controller's USB ids, from the id the browser gives it: Chrome's
 *  "... (Vendor: 054c Product: 0ce6)", Firefox's "054c-0ce6-...". */
function gamepadIds(id) {
  const m = /Vendor: ([0-9a-f]{4}) Product: ([0-9a-f]{4})/i.exec(id) ?? /^([0-9a-f]{1,4})-([0-9a-f]{1,4})-/i.exec(id);
  return m ? [parseInt(m[1], 16), parseInt(m[2], 16)] : [0, 0];
}

/** The id without the browser's notes: its name. */
function gamepadName(id) {
  return String(id).replace(/\s*\((?:[^()]*(?:Vendor|STANDARD GAMEPAD)[^()]*)\)\s*$/i, '').replace(/^[0-9a-f]{1,4}-[0-9a-f]{1,4}-/i, '').trim() || 'Gamepad';
}

/**
 * `Gamepads`: the browser's game controllers as the guest's
 * `/dev/input/event0` to `event3`, Linux evdev joysticks that SDL (and Wine
 * through SDL) read. A controller the browser maps to the standard layout
 * appears as an Xbox 360 pad, which games and SDL's mapping database know;
 * any other as a joystick under its own name. A browser shows a page a
 * controller only after a button on it is pressed. One per system, made any
 * time.
 *
 * Programs without udev find controllers by scanning `/dev/input` (SDL
 * looks again every few seconds); `env` holds what tells SDL to do that even
 * where libudev is installed: add it to `spawn`'s environment.
 */
export class Gamepads extends EventTarget {
  #i;
  #source;
  #mem = null;
  #at = 0;
  #slots = [];
  #timer = 0;
  #raf = 0;
  #closed = false;
  #k = null;
  /** The single-thread build's table is its Worker's: `#mem` is then a copy
   *  (at 0), sent back to `{ at, write }` with each change. */
  #remote = null;

  /** `source`: what lists the controllers (default `navigator.getGamepads`).
   *  Events: `connect` (with `slot`, N of `eventN`, and the `gamepad`) and
   *  `disconnect` (with `slot`, and `gamepad` as `{ id, mapping }`). */
  constructor(sys, { source } = {}) {
    super();
    this.#source = source ?? (() => globalThis.navigator?.getGamepads?.() ?? []);
    const i = internals(sys, 'Gamepads');
    this.#i = i;
    i.attach('gamepads', this);
    i.afterBoot(async (s) => {
      this.#k = i.kernel();
      let at;
      let mem = s.kernel.memory;
      if (s.joy) {
        const t = await s.joy.table();
        at = t.at;
        if (at) {
          this.#remote = { at, write: s.joy.write };
          mem = { buffer: t.words.buffer };
        }
      } else {
        at = (await this.#k.r('k_joy_table')) >>> 0;
      }
      if (this.#closed) return;
      if (!at) {
        i.say('gex: Gamepads: this kernel has no game controller table\n');
        return;
      }
      this.#mem = mem;
      this.#at = this.#remote ? 0 : at;
      const pads = this.#words()[(this.#at >>> 2) + 2];
      this.#slots = Array.from({ length: pads }, () => null);
      this.#loop();
    });
  }

  /** What `spawn` should add to a program's environment. */
  get env() {
    return { SDL_JOYSTICK_DISABLE_UDEV: '1' };
  }

  /** The controllers the guest has now: `{ slot, id, mapping }`. */
  get pads() {
    return this.#slots.flatMap((p, slot) => (p ? [{ slot, id: p.id, mapping: p.mapping }] : []));
  }

  #words() {
    return new Int32Array(this.#mem.buffer);
  }

  #loop() {
    const tick = () => {
      if (this.#closed) return;
      try { this.poll(); } catch (e) { this.#i.say(`gex: Gamepads: ${e?.message ?? e}\n`); }
      if (globalThis.requestAnimationFrame && globalThis.document?.visibilityState === 'visible') this.#raf = requestAnimationFrame(tick);
      else this.#timer = setTimeout(tick, 16);
    };
    tick();
  }

  /** Look at the controllers now (done every frame anyway). */
  poll() {
    if (!this.#mem || this.#closed) return;
    const list = [...(this.#source() ?? [])];
    const seen = new Set();
    let changed = false;
    for (const g of list) {
      if (!g || !g.connected || g.index >= this.#slots.length) continue;
      seen.add(g.index);
      changed = this.#put(g.index, g) || changed;
    }
    for (let slot = 0; slot < this.#slots.length; slot++) {
      if (this.#slots[slot] && !seen.has(slot)) changed = this.#drop(slot) || changed;
    }
    if (changed) this.#notify();
  }

  #pad(slot) {
    return (this.#at >>> 2) + J_HEADER + slot * 64;
  }

  /** Write a pad, if anything moved: whether it did. */
  #put(slot, g) {
    const w = new Int32Array(64);
    const old = this.#slots[slot];
    const fresh = !old || old.id !== g.id || old.mapping !== g.mapping;
    const nb = Math.min(g.buttons.length, MAX_BUTTONS);
    const na = Math.min(g.axes.length, MAX_AXES);
    w[P_KIND] = g.mapping === 'standard' ? 1 : 2;
    const [vendor, product] = gamepadIds(g.id);
    w[P_ID] = (vendor << 16) | product;
    w[P_COUNTS] = nb | (na << 8);
    let pressed = 0;
    for (let b = 0; b < nb; b++) {
      const btn = g.buttons[b];
      if (btn.pressed) pressed |= 1 << b;
      const v = Math.round(Math.max(0, Math.min(1, btn.value ?? (btn.pressed ? 1 : 0))) * 255);
      w[P_VALUES + (b >> 2)] |= v << ((b & 3) * 8);
    }
    w[P_PRESSED] = pressed;
    for (let a = 0; a < na; a++) w[P_AXES + a] = Math.round(Math.max(-1, Math.min(1, g.axes[a] || 0)) * 32767);
    const name = new TextEncoder().encode(gamepadName(g.id)).subarray(0, NAME_BYTES - 1);
    const nb8 = new Uint8Array(NAME_BYTES);
    nb8.set(name);
    const nw = new Int32Array(nb8.buffer);
    w.set(nw, P_NAME);
    if (!fresh && old.words.every((v, k) => k === P_GEN || k === P_SERIAL || k >= P_TIME || v === w[k])) return false;
    const mem = this.#words();
    const base = this.#pad(slot);
    w[P_GEN] = fresh ? (mem[base + P_GEN] + 1) | 0 : mem[base + P_GEN];
    const now = Date.now();
    w[P_TIME] = now % 0x100000000 | 0;
    w[P_TIME + 1] = Math.floor(now / 0x100000000);
    Atomics.add(mem, base + P_SERIAL, 1);
    for (let k = 1; k < 64; k++) Atomics.store(mem, base + k, w[k]);
    Atomics.add(mem, base + P_SERIAL, 1);
    this.#slots[slot] = { id: g.id, mapping: g.mapping, words: w };
    if (fresh) {
      Atomics.store(mem, (this.#at >>> 2) + J_CHANGED, Math.floor(now / 1000));
      this.dispatchEvent(Object.assign(new Event('connect'), { slot, gamepad: g }));
    }
    return true;
  }

  /** A controller went: its node goes. */
  #drop(slot) {
    const mem = this.#words();
    const base = this.#pad(slot);
    Atomics.add(mem, base + P_SERIAL, 1);
    Atomics.store(mem, base + P_KIND, 0);
    Atomics.add(mem, base + P_SERIAL, 1);
    Atomics.store(mem, (this.#at >>> 2) + J_CHANGED, Math.floor(Date.now() / 1000));
    const g = this.#slots[slot];
    this.#slots[slot] = null;
    this.dispatchEvent(Object.assign(new Event('disconnect'), { slot, gamepad: { id: g.id, mapping: g.mapping } }));
    return true;
  }

  /** A change in the table, which wakes the processes reading /dev/input. */
  #notify() {
    if (this.#remote) return void this.#remote.write(this.#remote.at, this.#words().slice());
    this.#k?.r('k_joy_notify').catch(() => {});
  }

  /** Stop: every controller goes from the guest. */
  close() {
    if (this.#closed) return;
    let changed = false;
    if (this.#mem) for (let slot = 0; slot < this.#slots.length; slot++) if (this.#slots[slot]) changed = this.#drop(slot) || changed;
    if (changed) this.#notify();
    this.#closed = true;
    clearTimeout(this.#timer);
    if (this.#raf) cancelAnimationFrame(this.#raf);
  }
}

/**
 * `WineControllers`: game controllers for Wine's programs, through
 * `gamepads` (a `Gamepads` of the same system). Wine's HID bus driver,
 * winebus (its SDL bus reads `/dev/input`), does not start when Wine boots:
 * a Wine install with controllers (wine64-dist.sh) marks its service
 * "DemandStart", so a session with no controller runs no driver for it and
 * fetches no SDL. This starts it, `sc start winebus` in the Wine session
 * (`wine`, the Wine loader's path, with `env`, the session's environment:
 * its WINEPREFIX), the first time the system has a controller: at the
 * first `connect`, or at once when one is already there. Once per system;
 * after a failure, again at the next `connect`. The games then see the
 * controller arrive (XInput, DirectInput, winmm's joystick calls).
 * Events: `start` (`code`: sc's exit status, 0 when the driver started).
 */
export class WineControllers extends EventTarget {
  #sys;
  #wine;
  #env;
  #gamepads;
  #state = 'idle'; // 'idle', 'starting', 'started'
  #onConnect = () => this.#start();
  #closed = false;

  constructor(sys, gamepads, { wine, env = [] } = {}) {
    super();
    if (!(gamepads instanceof Gamepads)) throw new TypeError('WineControllers takes a Gamepads as its second argument');
    if (typeof wine !== 'string' || !wine) throw new TypeError('WineControllers: wine, the Wine loader\'s path');
    internals(sys, 'WineControllers');
    this.#sys = sys;
    this.#wine = wine;
    this.#env = Array.isArray(env) ? env.map(String) : Object.entries(env).map(([k, v]) => `${k}=${v}`);
    this.#gamepads = gamepads;
    gamepads.addEventListener('connect', this.#onConnect);
    if (gamepads.pads.length) this.#start();
  }

  /** Whether winebus has been started: 'idle', 'starting' or 'started'. */
  get state() {
    return this.#state;
  }

  async #start() {
    if (this.#closed || this.#state !== 'idle') return;
    this.#state = 'starting';
    let code = -1;
    try {
      // sc's status is the start's error. ERROR_SERVICE_ALREADY_RUNNING
      // (1056, 32 as an exit status) is a start too: a program started it.
      const r = await this.#sys.run([this.#wine, 'sc', 'start', 'winebus'], { env: this.#env });
      code = r.code === (1056 & 0xff) ? 0 : r.code;
    } catch {
      code = -1;
    }
    this.#state = code === 0 ? 'started' : 'idle';
    this.dispatchEvent(Object.assign(new Event('start'), { code }));
  }

  /** Stop listening (the driver, once started, stays). */
  close() {
    this.#closed = true;
    this.#gamepads.removeEventListener('connect', this.#onConnect);
  }
}
