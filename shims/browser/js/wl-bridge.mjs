// The guest's Wayland compositor output, carried from a Worker to the page's
// canvas. The compositor state machine runs in the Worker (Rust shim); the
// canvas is on the page. When a client commits a frame the shim calls
// `wl_present` synchronously inside the scheduler, which copies the frame's
// RGBA pixels into a SharedArrayBuffer and wakes the page. One-way -- unlike
// the GL/virgl bridges there is no reply to wait for -- so no `Atomics.wait`.
//
// Mirrors gl-bridge/virgl-bridge in shape; the frame is small enough (a few MB)
// to copy each commit, and copying keeps the page off the guest's live memory.

// The largest frame the ring holds. A demo canvas is 300-640px; this is roomy
// and fixed so the buffers are allocated once.
const MAX_W = 2048;
const MAX_H = 2048;
export const WL_FB_BYTES = MAX_W * MAX_H * 4;

// meta words: [width, height, seq] -- `seq` bumps on every new frame so the
// page redraws only when there is something new.
const M_W = 0;
const M_H = 1;
const M_SEQ = 2;

// The input ring: the page appends mouse/keyboard events from its listeners and
// the Worker drains them into wasm memory during the compositor pump. A single
// producer (the page) and single consumer (the one Worker that runs the
// compositor), so two monotonic counters and a slot array are enough -- no
// locking, just a published head. Each event is five int32s: [kind, a, b, c, d]
// matching `wl_poll_input`.
const IN_CAP = 512; // events; a stall drops the oldest rather than blocking
const IN_STRIDE = 5;
const IN_HEAD = 0; // next slot the page will write (monotonic)
const IN_TAIL = 1; // next slot the Worker will read (monotonic)
const IN_SLOTS = 2; // first data word
const IN_BYTES = (IN_SLOTS + IN_CAP * IN_STRIDE) * 4;

/** The shared buffers a bridge is made of. */
export function makeWlChannel() {
  return {
    meta: new SharedArrayBuffer(16),
    fb: new SharedArrayBuffer(WL_FB_BYTES),
    input: new SharedArrayBuffer(IN_BYTES),
  };
}

/**
 * The page's half: draw the newest frame onto a 2D canvas. Call the returned
 * function on each `{kind:'wl'}` message; the pixels are already in shared
 * memory by the time it arrives.
 */
export function serveWl(canvas, channel) {
  const ctx = canvas.getContext('2d');
  const meta = new Int32Array(channel.meta);
  const fb = new Uint8Array(channel.fb);
  let lastSeq = 0;
  return function serve() {
    const seq = Atomics.load(meta, M_SEQ);
    if (seq === lastSeq) return;
    lastSeq = seq;
    const w = meta[M_W];
    const h = meta[M_H];
    if (w <= 0 || h <= 0 || w > MAX_W || h > MAX_H) return;
    if (canvas.width !== w) canvas.width = w;
    if (canvas.height !== h) canvas.height = h;
    // A copy off the shared buffer: `ImageData` rejects a view onto a
    // SharedArrayBuffer, and the page must not race the Worker's next write.
    const out = new Uint8ClampedArray(w * h * 4);
    out.set(fb.subarray(0, w * h * 4));
    ctx.putImageData(new ImageData(out, w, h), 0, 0);
  };
}

/** The Worker's half: `present`, what `Gex.useWl` calls from `wl_present`. */
export class WlProxy {
  #meta;
  #fb;
  #wake;
  #in;

  constructor(channel, wake) {
    this.#meta = new Int32Array(channel.meta);
    this.#fb = new Uint8Array(channel.fb);
    this.#wake = wake;
    // May be absent on an older channel; input is then simply never delivered.
    this.#in = channel.input ? new Int32Array(channel.input) : null;
  }

  /**
   * Copy a frame's pixels (tightly-packed RGBA at `ptr` in the guest `memory`)
   * into the shared framebuffer and wake the page. `stride` is `w*4` here.
   */
  present(memory, ptr, w, h, stride, _format) {
    const n = Math.min(stride * h, this.#fb.length);
    this.#fb.set(new Uint8Array(memory.buffer, ptr, n), 0);
    this.#meta[M_W] = w;
    this.#meta[M_H] = h;
    Atomics.add(this.#meta, M_SEQ, 1);
    this.#wake();
  }

  /**
   * Drain up to `max` input events the page has queued into wasm `memory` at
   * `ptr`, as packed i32 quintuples. Returns how many were written. Called from
   * the compositor pump (`wl_poll_input`).
   */
  pollInput(memory, ptr, max) {
    const ring = this.#in;
    if (!ring) return 0;
    const head = Atomics.load(ring, IN_HEAD);
    let tail = Atomics.load(ring, IN_TAIL);
    // If the page outran the drain by more than the ring holds, skip what was
    // overwritten -- an input event lost to a stall is not worth blocking for.
    if (head - tail > IN_CAP) tail = head - IN_CAP;
    const out = new Int32Array(memory.buffer, ptr, max * IN_STRIDE);
    let count = 0;
    while (tail < head && count < max) {
      const slot = IN_SLOTS + (tail % IN_CAP) * IN_STRIDE;
      const o = count * IN_STRIDE;
      out[o] = ring[slot];
      out[o + 1] = ring[slot + 1];
      out[o + 2] = ring[slot + 2];
      out[o + 3] = ring[slot + 3];
      out[o + 4] = ring[slot + 4];
      tail++;
      count++;
    }
    Atomics.store(ring, IN_TAIL, tail);
    return count;
  }
}

// KeyboardEvent.code -> Linux evdev keycode (input-event-codes.h). Wayland's
// wl_keyboard.key carries the evdev code; the client adds 8 for its xkb keymap.
// Enough of a US keyboard to type and to drive a toolkit; unmapped keys drop.
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

// xkb modifier-mask bits, matching the "complete" types the keymap includes.
const MOD_SHIFT = 1;
const MOD_LOCK = 2; // Caps
const MOD_CONTROL = 4;
const MOD_MOD1 = 8; // Alt
const MOD_MOD4 = 64; // Super/Meta

/**
 * The page's half of input: attach mouse and keyboard listeners to `canvas`
 * and append their events to the ring in `channel`. Returns a function that
 * removes the listeners again. Pointer coordinates are mapped from the canvas
 * element to the compositor's surface pixels. Call `now` for timestamps
 * (defaults to `performance.now`).
 */
export function attachWlInput(canvas, channel, now = () => performance.now()) {
  if (!channel.input) return () => {};
  const ring = new Int32Array(channel.input);
  const meta = new Int32Array(channel.meta);
  const push = (kind, a, b, c, d) => {
    const head = Atomics.load(ring, IN_HEAD);
    const slot = IN_SLOTS + (head % IN_CAP) * IN_STRIDE;
    ring[slot] = kind | 0;
    ring[slot + 1] = a | 0;
    ring[slot + 2] = b | 0;
    ring[slot + 3] = c | 0;
    ring[slot + 4] = d | 0;
    Atomics.store(ring, IN_HEAD, head + 1); // publish last
  };
  // Canvas element pixels differ from the surface's when CSS scales it; map
  // through the ratio so a click lands where it looks like it does. The surface
  // size is what the compositor last presented (meta), falling back to the
  // element's backing store.
  const toSurface = (e) => {
    const rect = canvas.getBoundingClientRect();
    const sw = meta[M_W] || canvas.width || rect.width;
    const sh = meta[M_H] || canvas.height || rect.height;
    const x = ((e.clientX - rect.left) / Math.max(1, rect.width)) * sw;
    const y = ((e.clientY - rect.top) / Math.max(1, rect.height)) * sh;
    return [Math.round(x), Math.round(y)];
  };
  const mods = (e) =>
    (e.shiftKey ? MOD_SHIFT : 0) |
    (e.ctrlKey ? MOD_CONTROL : 0) |
    (e.altKey ? MOD_MOD1 : 0) |
    (e.metaKey ? MOD_MOD4 : 0) |
    (e.getModifierState && e.getModifierState('CapsLock') ? MOD_LOCK : 0);
  // Browser mouse buttons (0 left, 1 middle, 2 right) to Linux BTN_* codes.
  const BTN = { 0: 0x110, 1: 0x112, 2: 0x111 };

  const onMove = (e) => {
    const [x, y] = toSurface(e);
    push(0, x, y, now() | 0, 0);
  };
  const onDown = (e) => {
    canvas.focus();
    const btn = BTN[e.button];
    if (btn === undefined) return;
    push(1, btn, 1, now() | 0, 0);
    e.preventDefault();
  };
  const onUp = (e) => {
    const btn = BTN[e.button];
    if (btn === undefined) return;
    push(1, btn, 0, now() | 0, 0);
    e.preventDefault();
  };
  const onWheel = (e) => {
    // 10 units per notch, in the Wayland axis sign (down is positive).
    const dy = Math.sign(e.deltaY) * 10;
    if (dy) push(2, 0, dy, now() | 0, 0);
    const dx = Math.sign(e.deltaX) * 10;
    if (dx) push(2, 1, dx, now() | 0, 0);
    e.preventDefault();
  };
  const onKey = (down) => (e) => {
    const code = EVDEV[e.code];
    if (code === undefined) return;
    // Modifiers first, so the client's xkb state is right when it reads the key.
    push(4, mods(e), 0, 0, 0);
    push(3, code, down ? 1 : 0, now() | 0, 0);
    e.preventDefault();
  };
  const onCtx = (e) => e.preventDefault(); // right-click is a button, not a menu

  // The canvas must be focusable to receive key events at all.
  if (!canvas.hasAttribute('tabindex')) canvas.setAttribute('tabindex', '0');
  const onDownKey = onKey(true);
  const onUpKey = onKey(false);
  canvas.addEventListener('mousemove', onMove);
  canvas.addEventListener('mousedown', onDown);
  window.addEventListener('mouseup', onUp); // a drag can release off-canvas
  canvas.addEventListener('wheel', onWheel, { passive: false });
  canvas.addEventListener('contextmenu', onCtx);
  canvas.addEventListener('keydown', onDownKey);
  canvas.addEventListener('keyup', onUpKey);
  return () => {
    canvas.removeEventListener('mousemove', onMove);
    canvas.removeEventListener('mousedown', onDown);
    window.removeEventListener('mouseup', onUp);
    canvas.removeEventListener('wheel', onWheel);
    canvas.removeEventListener('contextmenu', onCtx);
    canvas.removeEventListener('keydown', onDownKey);
    canvas.removeEventListener('keyup', onUpKey);
  };
}
