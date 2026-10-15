// The compositor's half of the guest: what a Wayland client's surfaces are, and
// where their pixels live, carried from every guest process to the one worker
// that draws them.
//
// Nothing here copies a frame. A `wl_shm` pool is guest memory mapped shared,
// so a surface's pixels are already in a `SharedArrayBuffer` the compositor can
// read: what crosses this channel is a *descriptor* per surface -- address,
// stride, size, role, and the rectangle that changed -- and the compositor
// uploads the damaged rows straight out of the guest's memory into a texture.
// The old path copied every frame four times (out of the pool, into a staging
// buffer, out of it again, and into a canvas) and could only ever show one
// window.
//
// There is one channel per page and a compositor in every guest process (see
// `mount_devices`), so the layout is a small arena: each process claims a block
// and is the only writer of it, plus a ring the compositor writes input into.
// No locks -- one writer per word, and the compositor polls at frame rate.

import { single, sched } from './single.mjs';

/** Processes that can publish surfaces at once. */
const BLOCKS = 8;
/** Surfaces one process can have on screen. */
export const SLOTS = 64;
/** Words in one surface record. Must match `REC` in `shims/browser/src/wl.rs`. */
export const REC = 78;
/** Words in one input event. Must match `EV` in `shims/browser/src/wl.rs`. */
export const EV = 8;
/** Events a process's input ring holds before the oldest is dropped. */
const RING = 64;

// Field offsets inside a surface record, named so neither side indexes by
// accident. The tail is the title as UTF-8 bytes.
export const R_CLIENT = 0;
export const R_SURFACE = 1;
export const R_ROLE = 2;
export const R_PARENT = 3;
export const R_REL_X = 4;
export const R_REL_Y = 5;
export const R_WIDTH = 6;
export const R_HEIGHT = 7;
export const R_ADDR = 8;
export const R_STRIDE = 9;
export const R_FORMAT = 10;
export const R_SERIAL = 11;
export const R_DAMAGE_X = 12;
export const R_DAMAGE_Y = 13;
export const R_DAMAGE_W = 14;
export const R_DAMAGE_H = 15;
export const R_FLAGS = 16;
/** The X window this surface is, or zero. See `F_EXTERNAL`. */
export const R_XID = 17;
export const R_TITLE = 18;
export const R_TITLE_WORDS = 8;
/** With `F_EXTERNAL`: where in the surface the host's GL frame goes, as
 * `x | y << 16` and `w | h << 16`. Zero for the whole surface (a Wayland GL
 * client's window is all frame; a Windows one has its frame and title bar
 * drawn by the program, and the GL picture only in the client area). */
export const R_GL_ORIGIN = 26;
export const R_GL_SIZE = 27;
/** Where the client placed the surface (`gex_window_placement_v1`, and every
 * X11 override-redirect window): `P_PLACED | P_UNMANAGED | layer << 8`. A
 * placed surface is at `R_REL_X`/`R_REL_Y` in output pixels. */
export const R_PLACE = 28;
/** The client's stacking name for the surface (a Win32 window, an X window). */
export const R_STACK_ID = 29;
/** The last restack: directly below the surface with this stacking name, 0
 * the top of its layer, -1 the bottom. Acted on when `R_RESTACK` changes. */
export const R_STACK_BELOW = 30;
export const R_RESTACK = 31;
/** Bumped when the client asks for this surface to have the keyboard. */
export const R_ACTIVATE = 32;
/** With `F_INPUT_REGION`: the bounds of where the surface takes the pointer
 * (`wl_surface.set_input_region`), in surface units. Zero-sized: nowhere. */
export const R_INPUT_X = 33;
export const R_INPUT_Y = 34;
export const R_INPUT_W = 35;
export const R_INPUT_H = 36;
/** Glass (`gex_glass_v1`): rectangle count, colorization ARGB, balance
 *  (bits 0..7), blur balance (8..15) and afterglow balance (16..23), then
 *  x, y, w, h per rectangle; the afterglow ARGB at `R_GLASS_AFTERGLOW`. */
export const R_GLASS = 37;
export const R_GLASS_ARGB = 38;
export const R_GLASS_BALANCE = 39;
export const R_GLASS_RECTS = 40;
export const GLASS_RECTS = 4;
/** The frame the compositor draws (`set_frame`): flags, then close, max and
 *  min buttons as x, y, w, h, state. */
export const R_GLASS_FRAME = 56;
export const R_GLASS_AFTERGLOW = 72;
/** With a frame: the client area, x, y, w, h; outside it the frame is drawn
 *  over the surface. */
export const R_GLASS_CLIENT = 73;
/** The caption's icon (`gex_glass_v1.set_icon`): the address in the process's
 *  memory of 16 x 16 premultiplied ARGB8888 pixels, 0 for none. */
export const R_GLASS_ICON = 77;
export const GLASS_FRAME_ON = 1;
export const GLASS_FRAME_ACTIVE = 2;
export const GLASS_FRAME_MAXIMIZED = 4;
export const GLASS_FRAME_TOOL = 8;
export const P_PLACED = 1;
export const P_UNMANAGED = 2;

/** Roles, as `gex_wayland::Role::code` writes them. */
export const ROLE_TOPLEVEL = 1;
export const ROLE_POPUP = 2;
export const ROLE_SUBSURFACE = 3;
export const ROLE_CURSOR = 4;

/** `R_FLAGS` bits (`gex_wayland::SurfaceInfo::flags`). Bit 0 was a move
 *  request, a flag nobody cleared; a request is now its kind, edges and
 *  number (bits 3..6 the number, 24..30 the request). */
export const F_CONFIGURE_PENDING = 4;
/** The pixels are not at `addr`: the host has them. A client that renders
 *  through the host's OpenGL attaches no buffer -- its frames never come back
 *  to the guest -- so the record says how big the surface is and the
 *  compositor finds the picture elsewhere. */
export const F_EXTERNAL = 2;
/** The client set an input region (`R_INPUT_X`..): without it the whole
 *  surface takes the pointer. */
export const F_INPUT_REGION = 128;

/** Event kinds the compositor writes into a process's ring. */
export const EV_MOTION = 0;
export const EV_BUTTON = 1;
export const EV_AXIS = 2;
export const EV_KEY = 3;
export const EV_MODS = 4;
export const EV_POINTER_CLEAR = 5;
export const EV_KEYBOARD_FOCUS = 6;
export const EV_CONFIGURE = 7;
export const EV_CLOSE = 8;
export const EV_DISMISS_POPUP = 9;
export const EV_MOVED = 10;

// -- layout -----------------------------------------------------------------
//
// meta: what the compositor decided, read by every process.
const M_OUT_W = 0;
const M_OUT_H = 1;
/** Bumped and notified by every publish, so a compositor can draw on the
 * publish instead of at its next animation frame (`waitPublish`). */
const M_PUB = 2;
/** Device pixels per surface unit for guest Wayland clients (wl_output.scale). */
const M_SCALE = 3;
const M_WORDS = 8;

// Block header: the token that claimed it, a heartbeat so a dead process's
// block can be reclaimed, how many surfaces it published, a generation that
// counts publishes, and the generation the compositor has drawn.
const B_TOKEN = 0;
const B_HEARTBEAT = 1;
const B_COUNT = 2;
const B_GEN = 3;
const B_ACK = 4;
/** Where the process's wait queue is in the global kernel's memory
 * (`gex_proc_queue_addr`), or zero, and where the wait slots' words start
 * (`gex_slots_addr`): what the compositor notifies after putting an event in
 * this block's ring (docs/kernel-locking.md §5.2 step 2). */
const B_WAKE = 5;
const B_SLOTS = 6;
const B_WORDS = 8;
const BLOCK_WORDS = B_WORDS + SLOTS * REC;

// Ring header: a monotonic write cursor and read cursor.
const K_HEAD = 0;
const K_TAIL = 1;
const K_WORDS = 2;
const RING_WORDS = K_WORDS + RING * EV;

const TOTAL_WORDS = M_WORDS + BLOCKS * (BLOCK_WORDS + RING_WORDS);

const blockBase = (b) => M_WORDS + b * BLOCK_WORDS;
const ringBase = (b) => M_WORDS + BLOCKS * BLOCK_WORDS + b * RING_WORDS;

/** The shared arena a compositor and its guests share. */
export function makeWlChannel() {
  // A plain buffer in the single-thread build, where the compositor and every
  // guest are one realm (docs/single-thread.md).
  const Buf = typeof SharedArrayBuffer === 'function' ? SharedArrayBuffer : ArrayBuffer;
  return { arena: new Buf(TOTAL_WORDS * 4) };
}

/** Tell the guests how large the output is, so a toplevel is configured to it. */
export function setOutputSize(channel, w, h, scale) {
  const a = new Int32Array(channel.arena);
  Atomics.store(a, M_OUT_W, w | 0);
  Atomics.store(a, M_OUT_H, h | 0);
  if (scale) Atomics.store(a, M_SCALE, Math.max(1, scale | 0));
}

/**
 * Resolves when any process publishes after `seen` (the value of a previous
 * call, or undefined for "from now"), with the new value; null where there is
 * no `Atomics.waitAsync` (the caller keeps its frame clock alone).
 */
export async function waitPublish(channel, seen) {
  if (typeof Atomics.waitAsync !== 'function' || !(channel.arena instanceof SharedArrayBuffer)) return null;
  const a = new Int32Array(channel.arena);
  const v = seen ?? Atomics.load(a, M_PUB);
  const r = Atomics.waitAsync(a, M_PUB, v);
  if (r.async) await r.value;
  return Atomics.load(a, M_PUB);
}

export function outputSize(channel) {
  const a = new Int32Array(channel.arena);
  return { w: Atomics.load(a, M_OUT_W) || 640, h: Atomics.load(a, M_OUT_H) || 480,
    scale: Atomics.load(a, M_SCALE) || 1 };
}

const decoder = new TextDecoder();
/// One scratch buffer for titles rather than one per surface per frame: this
/// runs for every surface at frame rate.
const titleBytes = new Uint8Array(R_TITLE_WORDS * 4);

/**
 * Every surface every process has published, newest state, as plain objects.
 *
 * Called once per frame by the compositor. Reading is deliberately allocation-
 * light per surface and does no work at all for a block that has not changed
 * since the caller last acked it -- `changed` says which those were.
 */

export function readScene(channel, { onlyChanged = false } = {}) {
  const a = new Int32Array(channel.arena);
  const out = [];
  const blocks = [];
  const now = Date.now();
  for (let b = 0; b < BLOCKS; b++) {
    const base = blockBase(b);
    const token = Atomics.load(a, base + B_TOKEN);
    if (!token) continue;
    const gen = Atomics.load(a, base + B_GEN);
    const ack = Atomics.load(a, base + B_ACK);
    const beat = Atomics.load(a, base + B_HEARTBEAT);
    blocks.push({ block: b, token, gen, ack, beat, stale: now - beat > 5000 });
    if (onlyChanged && gen === ack) continue;
    const count = Math.min(Atomics.load(a, base + B_COUNT), SLOTS);
    for (let i = 0; i < count; i++) {
      const r = base + B_WORDS + i * REC;
      // The title's bytes stop at the first NUL, or fill the field.
      const bytes = titleBytes;
      for (let w = 0; w < R_TITLE_WORDS; w++) {
        const v = a[r + R_TITLE + w];
        bytes[w * 4] = v & 0xff;
        bytes[w * 4 + 1] = (v >>> 8) & 0xff;
        bytes[w * 4 + 2] = (v >>> 16) & 0xff;
        bytes[w * 4 + 3] = (v >>> 24) & 0xff;
      }
      let end = bytes.indexOf(0);
      if (end < 0) end = bytes.length;
      out.push({
        token,
        block: b,
        client: a[r + R_CLIENT],
        surface: a[r + R_SURFACE],
        role: a[r + R_ROLE],
        parent: a[r + R_PARENT],
        relX: a[r + R_REL_X],
        relY: a[r + R_REL_Y],
        width: a[r + R_WIDTH],
        height: a[r + R_HEIGHT],
        addr: a[r + R_ADDR] >>> 0,
        stride: a[r + R_STRIDE],
        format: a[r + R_FORMAT],
        serial: a[r + R_SERIAL] >>> 0,
        damage: {
          x: a[r + R_DAMAGE_X],
          y: a[r + R_DAMAGE_Y],
          w: a[r + R_DAMAGE_W],
          h: a[r + R_DAMAGE_H],
        },
        flags: a[r + R_FLAGS],
        xid: a[r + R_XID] >>> 0,
        external: (a[r + R_FLAGS] & F_EXTERNAL) !== 0,
        glRect: (a[r + R_FLAGS] & F_EXTERNAL) && a[r + R_GL_SIZE]
          ? {
            x: a[r + R_GL_ORIGIN] << 16 >> 16,
            y: a[r + R_GL_ORIGIN] >> 16,
            w: a[r + R_GL_SIZE] & 0xffff,
            h: a[r + R_GL_SIZE] >>> 16,
          }
          : null,
        title: end ? decoder.decode(bytes.subarray(0, end)) : '',
        placed: (a[r + R_PLACE] & P_PLACED) !== 0,
        unmanaged: (a[r + R_PLACE] & P_UNMANAGED) !== 0,
        layer: (a[r + R_PLACE] >> 8) & 0xff,
        stackId: a[r + R_STACK_ID] >>> 0,
        below: a[r + R_STACK_BELOW] >>> 0,
        restack: a[r + R_RESTACK] >>> 0,
        activate: a[r + R_ACTIVATE] >>> 0,
        input: (a[r + R_FLAGS] & F_INPUT_REGION)
          ? { x: a[r + R_INPUT_X], y: a[r + R_INPUT_Y], w: a[r + R_INPUT_W], h: a[r + R_INPUT_H] }
          : null,
        glass: glassOf(a, r),
      });
    }
  }
  return { surfaces: out, blocks };
}

/** A record's glass: null without any, else its rectangles (surface
 *  units) and DWM's colorization parameters. */
function glassOf(a, r) {
  const count = Math.min(a[r + R_GLASS], GLASS_RECTS);
  if (count <= 0) return null;
  const rects = [];
  for (let i = 0; i < count; i++) {
    const q = r + R_GLASS_RECTS + 4 * i;
    rects.push({ x: a[q], y: a[q + 1], w: a[q + 2], h: a[q + 3] });
  }
  const buttons = [];
  for (let i = 0; i < 3; i++) {
    const q = r + R_GLASS_FRAME + 1 + 5 * i;
    buttons.push({ x: a[q], y: a[q + 1], w: a[q + 2], h: a[q + 3], state: a[q + 4] });
  }
  return {
    rects,
    frame: a[r + R_GLASS_FRAME] >>> 0,
    close: buttons[0],
    max: buttons[1],
    min: buttons[2],
    argb: a[r + R_GLASS_ARGB] >>> 0,
    balance: a[r + R_GLASS_BALANCE] & 0xff,
    blurBalance: (a[r + R_GLASS_BALANCE] >> 8) & 0xff,
    afterglow: a[r + R_GLASS_AFTERGLOW] >>> 0,
    afterglowBalance: (a[r + R_GLASS_BALANCE] >> 16) & 0xff,
    client: { x: a[r + R_GLASS_CLIENT], y: a[r + R_GLASS_CLIENT + 1], w: a[r + R_GLASS_CLIENT + 2], h: a[r + R_GLASS_CLIENT + 3] },
    icon: a[r + R_GLASS_ICON] >>> 0,
  };
}

/**
 * A process has exited: its block's surfaces go, and the block is free.
 *
 * A process that ends normally gives its block back (`release`), but one that
 * is killed never runs that code, and its last published scene stayed on the
 * screen: foot after SIGKILL kept its window, over the xterm the next leg
 * tried to resize. The compositor learns of every exit ('drop-memory'), so it
 * clears what the process left. Compare-and-swap, so a block some new process
 * has already claimed is left alone.
 */
export function releaseToken(channel, token) {
  const a = new Int32Array(channel.arena);
  let n = 0;
  for (let b = 0; b < BLOCKS; b++) {
    const base = blockBase(b);
    if (!token || Atomics.load(a, base + B_TOKEN) !== token) continue;
    Atomics.store(a, base + B_COUNT, 0);
    if (Atomics.compareExchange(a, base + B_TOKEN, token, 0) === token) n++;
  }
  return n;
}

/** Say that everything published up to `gen` has been drawn. */
export function ackBlock(channel, block, gen) {
  const a = new Int32Array(channel.arena);
  Atomics.store(a, blockBase(block) + B_ACK, gen);
}

/** The global kernel's memory, for `sendEvent`'s wake. One per page. */
export function setKernelMemory(channel, memory) {
  if (memory && !channel.kernelMemory) channel.kernelMemory = memory;
}

/** Write one event into a process's ring, for its guest to pick up. */
export function sendEvent(channel, block, kind, a1 = 0, b1 = 0, c1 = 0, d1 = 0, client = 0, surface = 0) {
  const a = new Int32Array(channel.arena);
  const base = ringBase(block);
  const head = Atomics.load(a, base + K_HEAD);
  const slot = base + K_WORDS + (head % RING) * EV;
  a[slot] = kind | 0;
  a[slot + 1] = a1 | 0;
  a[slot + 2] = b1 | 0;
  a[slot + 3] = c1 | 0;
  a[slot + 4] = d1 | 0;
  a[slot + 5] = client | 0;
  a[slot + 6] = surface | 0;
  a[slot + 7] = 0;
  Atomics.store(a, base + K_HEAD, head + 1); // publish last
  // A test's record of what the page sent (tests/eject/w3-page.mjs's drag).
  globalThis.__gexEvLog?.push([kind, a1, b1, performance.now()]);
  // A mark for traces (`console.timeStamp` costs nothing unless tracing).
  console.timeStamp(`gex:ev ${kind} b${block}`);
  // And a notify on the process's wait queue: with every thread of the
  // process parked, nothing drains the ring otherwise. The thread that wakes
  // takes a scheduler turn, whose compositor pump delivers the event and
  // notifies the client's reader in turn.
  const q = Atomics.load(a, blockBase(block) + B_WAKE);
  const slots = Atomics.load(a, blockBase(block) + B_SLOTS);
  const k = channel.kernelMemory;
  if (q && slots && k) {
    notifyQueue(new Int32Array(k.buffer), q >>> 0, slots >>> 0);
    // The single-thread build: the fibers parked on those words look.
    if (single.on) sched.checkSlots();
  }
}

/** Queue words: a bitmap of the slots registered on it, 8 words
 *  (shims/kernel/src/wait.rs `Queue`). */
const Q_BITS = 8;

/**
 * Notify the wait queue at byte address `q` in the kernel's memory `w`, as
 * `k_wq_notify` does: bump and wake the word of every slot registered on it,
 * the slots' words starting at `slots`. Not the queues chained to it -- a
 * process queue has none.
 */
export function notifyQueue(w, q, slots) {
  for (let i = 0; i < Q_BITS; i++) {
    let bits = Atomics.load(w, (q >>> 2) + i) >>> 0;
    while (bits) {
      const b = 31 - Math.clz32(bits & -bits);
      bits &= bits - 1;
      const at = (slots >>> 2) + i * 32 + b;
      Atomics.add(w, at, 1);
      if (w.buffer instanceof SharedArrayBuffer) Atomics.notify(w, at);
    }
  }
}

/**
 * The guest's half: one per process. Claims a block of the arena, mirrors what
 * its compositor state machine publishes into it, and hands its ring's events
 * back to the shim.
 */
export class WlProxy {
  #a;
  #block = -1;
  #base = 0;
  #ring = 0;
  #token;
  // Per surface, the damage published but not yet drawn, so several publishes
  // between two frames accumulate rather than the last one winning.
  #pending = new Map();

  /**
   * `tokenFn` names this process in the arena -- its pid, which it does not
   * have yet when this is constructed (the compositor has to exist before the
   * process is loaded, or `wayland-0` is never bound). So the block is claimed
   * lazily, on the first publish, when the answer is real.
   */
  constructor(channel, wake, tokenFn) {
    this.#a = new Int32Array(channel.arena);
    this.#tokenFn = tokenFn;
    this.#wake = wake;
  }

  #tokenFn;
  /** Called with the token once it is known, so the host can send the memory. */
  onToken = null;
  /** This process's wait queue's address in the global kernel's memory and
   *  where the slot words start, as `{ addr, slots }`, or null; asked when
   *  the block is claimed. See `B_WAKE`. */
  wakeAddr = null;

  /** Claim a block if this process does not have one yet. Returns whether it has. */
  #ready() {
    if (this.#block >= 0) return true;
    const token = this.#tokenFn?.() | 0;
    if (!token) return false;
    this.#token = token;
    this.#claim();
    if (this.#block >= 0) this.onToken?.(token);
    return this.#block >= 0;
  }

  #wake;

  /**
   * Take a block. A free one has no token; a block whose process died has a
   * heartbeat that stopped, and reclaiming it is what keeps a page that has
   * run a hundred processes from running out.
   */
  #claim() {
    const now = Date.now();
    // A block this process already holds: every thread of a process shares one
    // token, and the second thread to reach here must join rather than claim.
    for (let b = 0; b < BLOCKS; b++) {
      const base = blockBase(b);
      if (Atomics.load(this.#a, base + B_TOKEN) !== this.#token) continue;
      this.#block = b;
      this.#base = base;
      this.#ring = ringBase(b);
      this.#noteWake();
      return;
    }
    for (let pass = 0; pass < 2; pass++) {
      for (let b = 0; b < BLOCKS; b++) {
        const base = blockBase(b);
        const held = Atomics.load(this.#a, base + B_TOKEN);
        const dead = pass === 1 && held && now - Atomics.load(this.#a, base + B_HEARTBEAT) > 5000;
        if (held && !dead) continue;
        if (Atomics.compareExchange(this.#a, base + B_TOKEN, held, this.#token) !== held) continue;
        this.#block = b;
        this.#base = base;
        this.#ring = ringBase(b);
        Atomics.store(this.#a, base + B_COUNT, 0);
        Atomics.store(this.#a, base + B_GEN, 0);
        Atomics.store(this.#a, base + B_ACK, 0);
        Atomics.store(this.#a, base + B_HEARTBEAT, now);
        this.#noteWake();
        return;
      }
    }
  }

  #noteWake() {
    const w = this.wakeAddr?.();
    if (w?.addr && w?.slots) {
      Atomics.store(this.#a, this.#base + B_SLOTS, w.slots | 0);
      Atomics.store(this.#a, this.#base + B_WAKE, w.addr | 0);
    }
  }

  get token() {
    return this.#token;
  }

  /** Whether this process got a block at all. */
  get ok() {
    return this.#block >= 0;
  }

  /** Publishes so far (the block's generation). */
  get generation() {
    return Atomics.load(this.#a, this.#base + B_GEN);
  }

  /** The last generation the compositor has drawn. */
  get drawnGeneration() {
    return Atomics.load(this.#a, this.#base + B_ACK);
  }

  /**
   * Mirror `count` records the shim wrote at `ptr` into this process's block.
   *
   * Damage is merged rather than replaced while the compositor has not drawn
   * the previous publish: the shim publishes on every scheduler turn and the
   * compositor draws once a frame, so several publishes commonly share one
   * frame and each carries only its own rectangle.
   */
  publish(memory, ptr, count) {
    if (!this.#ready()) return;
    const n = Math.min(count, SLOTS);
    const src = new Int32Array(memory.buffer, ptr, n * REC);
    const gen = Atomics.load(this.#a, this.#base + B_GEN);
    const drawn = Atomics.load(this.#a, this.#base + B_ACK) === gen;
    if (drawn) this.#pending.clear();
    for (let i = 0; i < n; i++) {
      const o = i * REC;
      const dst = this.#base + B_WORDS + o;
      for (let w = 0; w < REC; w++) this.#a[dst + w] = src[o + w];
      // Accumulate damage for a surface the compositor has not caught up with.
      const key = `${src[o + R_CLIENT]}:${src[o + R_SURFACE]}`;
      const held = drawn ? null : this.#pending.get(key);
      const rect = {
        x: src[o + R_DAMAGE_X],
        y: src[o + R_DAMAGE_Y],
        w: src[o + R_DAMAGE_W],
        h: src[o + R_DAMAGE_H],
      };
      const merged = held ? mergeDamage(held, rect) : rect;
      this.#pending.set(key, merged);
      this.#a[dst + R_DAMAGE_X] = merged.x;
      this.#a[dst + R_DAMAGE_Y] = merged.y;
      this.#a[dst + R_DAMAGE_W] = merged.w;
      this.#a[dst + R_DAMAGE_H] = merged.h;
    }
    Atomics.store(this.#a, this.#base + B_COUNT, n);
    Atomics.store(this.#a, this.#base + B_HEARTBEAT, Date.now());
    Atomics.add(this.#a, this.#base + B_GEN, 1);
    console.timeStamp(`gex:publish ${n} b${this.#block}`);
    Atomics.add(this.#a, M_PUB, 1);
    Atomics.notify(this.#a, M_PUB);
    this.#wake?.();
  }

  /**
   * Drain this process's input ring into wasm memory at `ptr`, as packed i32
   * records. Returns how many were written.
   */
  pollInput(memory, ptr, max) {
    if (!this.#ready()) return 0;
    const a = this.#a;
    const base = this.#ring;
    const head = Atomics.load(a, base + K_HEAD);
    let tail = Atomics.load(a, base + K_TAIL);
    // A stall longer than the ring drops the oldest events rather than
    // blocking: an input event no one took in a second is not worth having.
    if (head - tail > RING) tail = head - RING;
    const out = new Int32Array(memory.buffer, ptr, max * EV);
    let count = 0;
    while (tail < head && count < max) {
      const slot = base + K_WORDS + (tail % RING) * EV;
      for (let w = 0; w < EV; w++) out[count * EV + w] = a[slot + w];
      tail++;
      count++;
    }
    Atomics.store(a, base + K_TAIL, tail);
    if (count) console.timeStamp(`gex:poll ${count} b${this.#block}`);
    return count;
  }

  /** Whether this process's input ring holds an event. */
  inputPending() {
    if (this.#block < 0) return false;
    const a = this.#a;
    return Atomics.load(a, this.#ring + K_HEAD) !== Atomics.load(a, this.#ring + K_TAIL);
  }

  /** Give the block back, so a later process can have it. */
  release() {
    if (this.#block < 0) return;
    Atomics.store(this.#a, this.#base + B_COUNT, 0);
    Atomics.store(this.#a, this.#base + B_TOKEN, 0);
    this.#block = -1;
  }
}

/** The smallest rectangle covering both, with `w <= 0` meaning "everything". */
export function mergeDamage(a, b) {
  if (a.w <= 0 || a.h <= 0 || b.w <= 0 || b.h <= 0) return { x: 0, y: 0, w: 0, h: 0 };
  const x = Math.min(a.x, b.x);
  const y = Math.min(a.y, b.y);
  const r = Math.max(a.x + a.w, b.x + b.w);
  const bt = Math.max(a.y + a.h, b.y + b.h);
  return { x, y, w: r - x, h: bt - y };
}
