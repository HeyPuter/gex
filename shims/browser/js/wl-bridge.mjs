// The compositor's half of the guest: what a Wayland client's surfaces are, and
// where their pixels live, carried from every guest process to the one worker
// that draws them.
//
// What crosses this channel is a *descriptor* per surface -- where its pixels
// are, stride, size, role, and the rectangle that changed -- and the compositor
// uploads the damaged rows into a texture. The pixels are copied once, by the
// guest as it publishes, out of its `wl_shm` pool into its block's pixel
// buffer (`pixels`, a SharedArrayBuffer per block): the damaged rows
// only, and nothing for a surface whose commit has not moved on. The old path
// copied every frame four times (out of the pool, into a staging buffer, out
// of it again, and into a canvas) and could only ever show one window.
//
// Each process's proxy in each of its Workers has a buffer of its own, sized
// to what its surfaces need and replaced by a larger one when they outgrow
// it. A record names its buffer by number (`R_ADDR_HI`); the guest posts
// the compositor each new buffer ('wl-pixels'), and the compositor draws a
// surface once its buffer has come. Fixed in size once made, because
// `writeTexture` refuses a view of a growable buffer; and not made ahead for
// every block, because a buffer's whole length counts in the page's data
// size whether it is written or not.
//
// The compositor reads that buffer and not the guest's memory, because it
// must not hold the memory. V8 counts every wasm memory an isolate holds, at
// its length, in the size its next full collection is scheduled from; past
// about 8 GiB in all the headroom stops growing and it collects every few
// megabytes allocated. A memory64 game's memory alone can be past that, a
// Wine process's is about 2 GiB, and the compositor that held them all
// collected 25 times a second. A plain SharedArrayBuffer is not counted.
//
// There is one channel per page and a compositor in every guest process (see
// `mount_devices`), so the layout is a small arena: each process claims a block
// and is the only writer of it, plus a ring the compositor writes input into.
// No locks -- one writer per word, and the compositor polls at frame rate.

import { single, sched } from './single.mjs';

/** Processes that can publish surfaces at once (a desktop shell has more
 *  than eight: winlogon, the desktop, explorer, its COM servers). */
const BLOCKS = 32;
/** Surfaces one process can have on screen. */
export const SLOTS = 64;
/** Words in one surface record. Must match `REC` in `shims/browser/src/wl.rs`. */
export const REC = 38;
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
/** `R_ADDR`'s high half: a memory64 process's pool may sit above 4 GiB. */
export const R_ADDR_HI = 37;
/** A record's pixel address, both halves, as a Number (exact below 2^53). */
export const recordAddr = (a, r) => (a[r + R_ADDR] >>> 0) + (a[r + R_ADDR_HI] >>> 0) * 0x100000000;
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
/** The client asked to be fullscreen (`xdg_toplevel.set_fullscreen`): no
 *  frame, the whole output. A state, held until `unset_fullscreen`. */
export const F_FULLSCREEN = 1;
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
/** Set by the first process to hand the page the global kernel's memory, so
 *  every other one (and every thread) does not send another copy of it. */
const M_KERNEL_SENT = 4;
/** The wake relay (`startWakeRelay`): a compositor that does not hold the
 *  kernel's memory sets its block's bit in `M_RELAY_BLOCKS` after putting an
 *  event in the ring, and bumps and notifies `M_RELAY`; the thread that holds
 *  the kernel notifies those blocks' queues. */
const M_RELAY = 5;
const M_RELAY_BLOCKS = 6;
/** The last pixel buffer number handed out (`WlProxy`'s buffers). */
const M_PIXEL_IDS = 7;
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
 * this block's ring. */
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

// A pixel buffer: a header -- a lock, the next free byte, and a region per
// surface -- then the surfaces' pixels, packed rows of width * 4.
const P_LOCK = 0;
const P_NEXT = 1;
const P_ENTRIES = 4;
/** Per surface: client, surface, offset, size, the serial last copied. */
const PE = 5;
const P_DATA = 2048;
/** The largest pixel buffer: room for seven 1920x1080 surfaces. */
const PIXELS_MAX = 64 << 20;
/** Pixel buffers are made in steps of this. */
const PIXELS_STEP = 1 << 20;

/** The shared arena a compositor and its guests share (see the top of this
 *  file); `pixels` says the guests copy their surfaces' pixels into buffers
 *  of their own. */
export function makeWlChannel() {
  // A plain buffer in the single-thread build, where the compositor and every
  // guest are one realm -- and there no pixel buffers: the one isolate holds
  // the guests' memories whatever this does, and counts a plain ArrayBuffer.
  if (typeof SharedArrayBuffer !== 'function') return { arena: new ArrayBuffer(TOTAL_WORDS * 4) };
  return { arena: new SharedArrayBuffer(TOTAL_WORDS * 4), pixels: true };
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
  // With pixel buffers an address is an offset into the buffer the record
  // names (`pix`), not into the process's memory.
  const pixels = !!channel.pixels;
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
        addr: pixels ? a[r + R_ADDR] >>> 0 : recordAddr(a, r),
        pix: pixels ? a[r + R_ADDR_HI] >>> 0 : 0,
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
      });
    }
  }
  return { surfaces: out, blocks };
}

/**
 * Whether a surface takes the pointer anywhere: false only when its client
 * set an empty input region (`wl_surface.set_input_region` with nothing in
 * it), and the pointer then goes to what is under it. A client's subsurface
 * for a window's GL picture is one such surface, laid over its parent.
 */
export function takesPointer(record) {
  const r = record.input;
  return !r || (r.w > 0 && r.h > 0);
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

/** Where `sceneMark` keeps each block's token and generation. */
export function sceneMarks() {
  return new Int32Array(2 * BLOCKS);
}

/** One view of each arena, for the checks made every frame. */
const arenaViews = new WeakMap();
function arenaView(channel) {
  let a = arenaViews.get(channel.arena);
  if (!a) arenaViews.set(channel.arena, (a = new Int32Array(channel.arena)));
  return a;
}

/** Note what the compositor has just drawn (`readScene`'s `blocks`), for
 *  `sceneStill`. */
export function sceneMark(marks, blocks) {
  marks.fill(0);
  for (const b of blocks) {
    marks[2 * b.block] = b.token;
    marks[2 * b.block + 1] = b.gen;
  }
}

/** Whether no block has been claimed, freed or published to since `sceneMark`,
 *  and everything published has been acked: the scene `readScene` would
 *  read is the one already drawn. Reads the arena only, and allocates
 *  nothing. */
export function sceneStill(channel, marks) {
  const a = arenaView(channel);
  for (let b = 0; b < BLOCKS; b++) {
    const base = blockBase(b);
    const token = Atomics.load(a, base + B_TOKEN);
    if (token !== marks[2 * b]) return false;
    if (!token) continue;
    const gen = Atomics.load(a, base + B_GEN);
    if (gen !== marks[2 * b + 1] || gen !== Atomics.load(a, base + B_ACK)) return false;
  }
  return true;
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
  // A test's record of what the page sent.
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
  } else if (q && slots && channel.arena instanceof SharedArrayBuffer) {
    // No kernel memory here: the relay's thread has it (`startWakeRelay`).
    Atomics.or(a, M_RELAY_BLOCKS, 1 << block);
    Atomics.add(a, M_RELAY, 1);
    Atomics.notify(a, M_RELAY);
  }
}

/**
 * Notify, for a compositor that does not hold the global kernel's memory,
 * the wait queue of every block `sendEvent` marked (`M_RELAY_BLOCKS`). Runs
 * on the thread that has the kernel anyway (the page's), so the compositor
 * holds no kernel memory, which the engine would count against its heap at
 * the memory's whole length. Started once per channel; the first call's
 * memory is the one used, as `setKernelMemory`'s is.
 */
export function startWakeRelay(channel, kernel) {
  if (!kernel || channel.relay || typeof Atomics.waitAsync !== 'function' || !(channel.arena instanceof SharedArrayBuffer)) return;
  channel.relay = kernel;
  const a = new Int32Array(channel.arena);
  (async () => {
    for (;;) {
      const v = Atomics.load(a, M_RELAY);
      let bits = Atomics.exchange(a, M_RELAY_BLOCKS, 0) >>> 0;
      if (bits) {
        // Re-taken each time: the kernel's memory grows.
        const w = new Int32Array(kernel.buffer);
        while (bits) {
          const b = 31 - Math.clz32(bits & -bits);
          bits &= bits - 1;
          const q = Atomics.load(a, blockBase(b) + B_WAKE);
          const slots = Atomics.load(a, blockBase(b) + B_SLOTS);
          if (q && slots) notifyQueue(w, q >>> 0, slots >>> 0);
        }
      }
      const r = Atomics.waitAsync(a, M_RELAY, v);
      if (r.async) await r.value;
    }
  })();
}

/** Queue words: a bitmap of the slots registered on it, 64 words
 *  (shims/kernel/src/wait.rs `Queue`). */
const Q_BITS = 64;

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
    this.#pixels = !!channel.pixels;
    this.#tokenFn = tokenFn;
    this.#wake = wake;
  }

  #pixels;
  /** This proxy's pixel buffer and its number, or null before the first
   *  surface with pixels. */
  #buf = null;
  #bufId = 0;
  #saidTooBig = false;
  /** Called with `{ token, id, buffer, replaces }` for each new pixel buffer,
   *  for the host to send the compositor. */
  onPixels = null;

  /** A buffer of `bytes` in place of this proxy's, empty, sent to the
   *  compositor; or false when one that large is not made. */
  #newBuffer(bytes) {
    const size = Math.ceil(bytes / PIXELS_STEP) * PIXELS_STEP;
    if (size > PIXELS_MAX) return false;
    const replaces = this.#bufId;
    this.#buf = new SharedArrayBuffer(size);
    // Numbers are page-wide and never 0, which is a record without pixels.
    let id = (Atomics.add(this.#a, M_PIXEL_IDS, 1) + 1) >>> 0;
    if (!id) id = (Atomics.add(this.#a, M_PIXEL_IDS, 1) + 1) >>> 0;
    this.#bufId = id;
    this.onPixels?.({ token: this.#token, id: this.#bufId, buffer: this.#buf, replaces });
    return true;
  }

  /** Whether this is the first process to ask: it hands the page the
   *  kernel's memory (`M_KERNEL_SENT`). */
  firstForKernel() {
    return Atomics.compareExchange(this.#a, M_KERNEL_SENT, 0, 1) === 0;
  }

  /**
   * Copy the pixels of this publish's surfaces out of `memory` into the
   * block's pixel buffer and point their records (at `dst` in the arena) at
   * them: the damaged rows, all of them for a surface with a new region, and
   * none for one whose serial was copied already.
   */
  #copyPixels(memory, recs) {
    if (!this.#pixels) return;
    const a = this.#a;
    // A buffer that cannot take this publish's new regions is replaced by
    // one that can, with room to spare, and every surface is copied whole.
    // A first one is made for what this publish needs.
    {
      const h = this.#buf && new Int32Array(this.#buf, 0, P_DATA >> 2);
      let more = 0;
      let all = 0;
      for (const { dst } of recs) {
        const need = ((a[dst + R_WIDTH] * a[dst + R_HEIGHT] * 4) + 255) & ~255;
        all += need;
        if (!h) continue;
        let o = -1;
        for (let e = 0; e < SLOTS && o < 0; e++) {
          const q = P_ENTRIES + e * PE;
          if (h[q + 2] && h[q] === a[dst + R_CLIENT] && h[q + 1] === a[dst + R_SURFACE]) o = q;
        }
        if (o < 0 || h[o + 3] < need) more += need;
      }
      const used = h ? Math.max(h[P_NEXT], P_DATA) : P_DATA;
      if (!h || (used + more > this.#buf.byteLength && P_DATA + all > this.#buf.byteLength)) {
        const want = P_DATA + all;
        const grown = Math.max(want + (want >> 2), this.#buf ? this.#buf.byteLength * 2 : 0);
        if (!this.#newBuffer(Math.min(grown, PIXELS_MAX)) && !this.#buf) {
          for (const { dst } of recs) {
            a[dst + R_ADDR] = 0;
            a[dst + R_ADDR_HI] = 0;
          }
          return;
        }
      }
    }
    const buf = this.#buf;
    const h = new Int32Array(buf, 0, P_DATA >> 2);
    while (Atomics.compareExchange(h, P_LOCK, 0, 1) !== 0);
    try {
      if (h[P_NEXT] < P_DATA) h[P_NEXT] = P_DATA;
      const find = (c, sf) => {
        for (let e = 0; e < SLOTS; e++) {
          const o = P_ENTRIES + e * PE;
          if (h[o + 2] && h[o] === c && h[o + 1] === sf) return o;
        }
        return -1;
      };
      // The regions this publish needs; a buffer that cannot take the new
      // ones as well starts again from empty, every surface copied whole.
      let more = 0;
      for (const { dst } of recs) {
        const o = find(a[dst + R_CLIENT], a[dst + R_SURFACE]);
        const need = a[dst + R_WIDTH] * a[dst + R_HEIGHT] * 4;
        if (o < 0 || h[o + 3] < need) more += (need + 255) & ~255;
      }
      if (h[P_NEXT] + more > buf.byteLength) {
        for (let e = 0; e < SLOTS * PE; e++) h[P_ENTRIES + e] = 0;
        h[P_NEXT] = P_DATA;
      }
      const src = new Uint8Array(memory.buffer);
      for (const { dst, rect, mine: last } of recs) {
        const mine = last === this.#bufId;
        const w = a[dst + R_WIDTH];
        const ht = a[dst + R_HEIGHT];
        const need = w * ht * 4;
        const from = recordAddr(a, dst);
        const stride = a[dst + R_STRIDE];
        let o = find(a[dst + R_CLIENT], a[dst + R_SURFACE]);
        // Only the damage when this buffer has the surface's last commit:
        // another thread's proxy may have published the ones between.
        let whole = !mine;
        if (o < 0 || h[o + 3] < need) {
          if (o < 0) {
            for (let e = 0; e < SLOTS && o < 0; e++) if (!h[P_ENTRIES + e * PE + 2]) o = P_ENTRIES + e * PE;
            if (o < 0) o = P_ENTRIES;
          }
          const at = h[P_NEXT];
          // Bigger than the whole buffer: not shown, and said once.
          if (at + need > buf.byteLength) {
            if (!this.#saidTooBig) console.warn(`[wl] a ${w}x${ht} surface does not fit its block's ${buf.byteLength >> 20} MiB of pixels`);
            this.#saidTooBig = true;
            a[dst + R_ADDR] = 0;
            a[dst + R_ADDR_HI] = 0;
            continue;
          }
          h[o] = a[dst + R_CLIENT];
          h[o + 1] = a[dst + R_SURFACE];
          h[o + 2] = at;
          h[o + 3] = need;
          h[P_NEXT] = at + ((need + 255) & ~255);
          whole = true;
        }
        const at = h[o + 2];
        const serial = a[dst + R_SERIAL];
        if (whole || h[o + 4] !== serial) {
          let x = 0, y = 0, rw = w, rh = ht;
          if (!whole && rect.w > 0 && rect.h > 0) {
            x = Math.max(0, Math.min(rect.x, w));
            y = Math.max(0, Math.min(rect.y, ht));
            rw = Math.max(0, Math.min(rect.w, w - x));
            rh = Math.max(0, Math.min(rect.h, ht - y));
          }
          const out = new Uint8Array(buf, at, need);
          if (from + (y + rh - 1) * stride + (x + rw) * 4 <= src.length || rh === 0) {
            for (let r = 0; r < rh; r++) {
              const s0 = from + (y + r) * stride + x * 4;
              out.set(src.subarray(s0, s0 + rw * 4), (y + r) * w * 4 + x * 4);
            }
          }
          h[o + 4] = serial;
        }
        a[dst + R_ADDR] = at;
        a[dst + R_ADDR_HI] = this.#bufId;
        a[dst + R_STRIDE] = w * 4;
      }
    } finally {
      Atomics.store(h, P_LOCK, 0);
    }
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
        // The last process's regions are not this one's: its surfaces may
        // have the same numbers.
        if (this.#buf) new Int32Array(this.#buf, 0, P_DATA >> 2).fill(0);
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
   *
   * The records are read out of `memory`, and the pixels they point at out of
   * `pixels`: the same memory for a process's own shim, the X server's for
   * hostx, whose records come from its Wayland server's module.
   */
  publish(memory, ptr, count, pixels = memory) {
    // A block taken over as dead (no publish for 5 s: an idle process, not
    // only a killed one) is another process's now. Publishing into it put
    // this process's surfaces under that process's token, and the compositor
    // read them out of the wrong memory. Claim another and publish there.
    if (this.#block >= 0 && Atomics.load(this.#a, this.#base + B_TOKEN) !== this.#token) {
      this.#block = -1;
      this.#pending.clear();
    }
    if (!this.#ready()) return;
    const n = Math.min(count, SLOTS);
    const src = new Int32Array(memory.buffer, ptr, n * REC);
    const gen = Atomics.load(this.#a, this.#base + B_GEN);
    const drawn = Atomics.load(this.#a, this.#base + B_ACK) === gen;
    if (drawn) this.#pending.clear();
    const shm = [];
    for (let i = 0; i < n; i++) {
      const o = i * REC;
      const dst = this.#base + B_WORDS + o;
      // The buffer this slot's surface was last published from, if it is the
      // same surface.
      const same = this.#a[dst + R_CLIENT] === src[o + R_CLIENT] && this.#a[dst + R_SURFACE] === src[o + R_SURFACE];
      const mine = same && this.#pixels ? this.#a[dst + R_ADDR_HI] >>> 0 : 0;
      for (let w = 0; w < REC; w++) this.#a[dst + w] = src[o + w];
      if (this.#pixels && recordAddr(this.#a, dst) && src[o + R_WIDTH] > 0 && src[o + R_HEIGHT] > 0) {
        shm.push({ dst, mine, rect: { x: src[o + R_DAMAGE_X], y: src[o + R_DAMAGE_Y], w: src[o + R_DAMAGE_W], h: src[o + R_DAMAGE_H] } });
      }
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
    if (shm.length) this.#copyPixels(pixels, shm);
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
