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
import { SP_CODE } from './mode.mjs';

/** Processes that can publish surfaces at once (a desktop shell has more
 *  than eight: winlogon, the desktop, explorer, its COM servers). */
const BLOCKS = 32;
/** Surfaces one process can have on screen. */
export const SLOTS = 64;
/** Words in one surface record. Must match `REC` in `crates/gex-wayland`
 *  (`push_record`), which both the guest shim and the X server's bridge
 *  write. */
export const REC = 119;
/** Words in one input event. Must match `EV` in `hosts/browser/src/wl.rs`. */
export const EV = 8;
/** Events a process's input ring holds before the oldest is dropped. */
const RING = 64;

// Field offsets inside a surface record, named so neither side indexes by
// accident. The tail is the title, then the app id, as UTF-8 bytes, then the
// program's pid where its connection does not say it (an X window's).
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
// Words 18..25 are free (the title's, before it had room).
export const R_TITLE = 38;
export const R_TITLE_WORDS = 64;
export const R_APP_ID = 102;
export const R_APP_ID_WORDS = 16;
export const R_PID = 118;
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
/** A client asked to lock the pointer to this surface
 *  (`zwp_locked_pointer_v1`): the page locks it when it can. */
export const F_POINTER_LOCK = 1 << 31;

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
/** The page's drag of host files (page/src/drag.js): enters a surface at
 *  (x, y) with the page's serial for it (its data is `setDragData`'s),
 *  moves (x, y, time), is dropped (serial), leaves. */
export const EV_DRAG_ENTER = 11;
export const EV_DRAG_MOTION = 12;
export const EV_DROP = 13;
export const EV_DRAG_LEAVE = 14;
/** The page's pointer lock on a surface: a1 1 began, 0 ended. */
export const EV_POINTER_LOCK = 15;
/** Movement under the lock: a1, b1 in 1/256 pixel, c1 the time (ms). */
export const EV_RELATIVE = 16;
/** What the guest says of a drag (`B_DRAG`): a drop now would be taken; the
 *  drop was answered; taken; the client is done with it. */
export const DRAG_ACCEPTS = 1;
export const DRAG_ANSWERED = 2;
export const DRAG_TAKEN = 4;
export const DRAG_FINISHED = 8;

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
/** The page's drag, as the guest last said of it (`wl_drag_status`):
 *  `serial << 4 | DRAG_*`. Notified when it changes. */
const B_DRAG = 7;
const B_WORDS = 8;
const BLOCK_WORDS = B_WORDS + SLOTS * REC;

// Ring header: a monotonic write cursor and read cursor.
const K_HEAD = 0;
const K_TAIL = 1;
const K_WORDS = 2;
const RING_WORDS = K_WORDS + RING * EV;

// The page's drag data (one drag at a time a page): its length in bytes,
// then the bytes, a text/uri-list, set before the drag's enter is sent.
const D_BYTES = 64 << 10;
const D_WORDS = 1 + D_BYTES / 4;

// The clipboard (one a page, every process's clients and the page's own):
// a lock, a number that moves with each new text, who set it (0 the page,
// 1 a guest client, 2 a Wine client: one whose text Wine's other clients
// already have), the text's length, then its bytes (UTF-8).
const C_LOCK = 0;
const C_SERIAL = 1;
const C_FROM = 2;
const C_LEN = 3;
const C_HEAD = 4;
const C_BYTES = 1 << 20;
const C_WORDS = C_HEAD + C_BYTES / 4;

const TOTAL_WORDS = M_WORDS + BLOCKS * (BLOCK_WORDS + RING_WORDS) + D_WORDS + C_WORDS;
const dragBase = M_WORDS + BLOCKS * (BLOCK_WORDS + RING_WORDS);
const clipBase = dragBase + D_WORDS;

function clipLock(a) {
  while (Atomics.compareExchange(a, clipBase + C_LOCK, 0, 1) !== 0) {
    // Writers hold it for one copy of at most a megabyte.
    if (typeof Atomics.wait === 'function' && !globalThis.document) Atomics.wait(a, clipBase + C_LOCK, 1, 1);
  }
}
function clipUnlock(a) {
  Atomics.store(a, clipBase + C_LOCK, 0);
  Atomics.notify(a, clipBase + C_LOCK);
}

/** Put `bytes` on the clipboard, from the page (`from` 0) or a guest (1, or
 *  2 for a Wine client): its new number. */
function clipPut(a, bytes, from) {
  const n = Math.min(bytes.length, C_BYTES);
  clipLock(a);
  new Uint8Array(a.buffer, (clipBase + C_HEAD) * 4, n).set(bytes.subarray(0, n));
  Atomics.store(a, clipBase + C_LEN, n);
  Atomics.store(a, clipBase + C_FROM, from);
  const serial = (Atomics.add(a, clipBase + C_SERIAL, 1) + 1) >>> 0;
  clipUnlock(a);
  return serial;
}

/** The clipboard: `{ serial, from: 'page' | 'guest', text }` (serial 0:
 *  nothing yet). */
export function getClipboard(channel) {
  const a = new Int32Array(channel.arena);
  clipLock(a);
  const serial = Atomics.load(a, clipBase + C_SERIAL) >>> 0;
  const from = Atomics.load(a, clipBase + C_FROM) ? 'guest' : 'page';
  const n = Math.min(Atomics.load(a, clipBase + C_LEN) >>> 0, C_BYTES);
  const bytes = new Uint8Array(n);
  bytes.set(new Uint8Array(a.buffer, (clipBase + C_HEAD) * 4, n));
  clipUnlock(a);
  return { serial, from, text: new TextDecoder().decode(bytes) };
}

/** The clipboard's number alone, cheap to poll. */
export function clipboardSerial(channel) {
  return Atomics.load(new Int32Array(channel.arena), clipBase + C_SERIAL) >>> 0;
}

/** The page's text on the clipboard: every client with a data device is
 *  offered it. Its number. */
export function setClipboard(channel, text) {
  // On a mirror, the real channel's too (`mirrorChannel`).
  channel.mirror?.('setClipboard', [text]);
  const a = new Int32Array(channel.arena);
  const serial = clipPut(a, new TextEncoder().encode(String(text)), 0);
  // Every process's pump offers it, idle ones too.
  for (let b = 0; b < BLOCKS; b++) if (Atomics.load(a, blockBase(b) + B_TOKEN)) wakeBlock(channel, a, b);
  return serial;
}

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

/** Whether `b` is shared memory: never, on a page that has none (the
 *  single-thread build's, where the name itself is not defined). */
const isShared = (b) => typeof SharedArrayBuffer === 'function' && b instanceof SharedArrayBuffer;

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
  // On a mirror, the real channel's too (`mirrorChannel`).
  channel.mirror?.('setOutputSize', [w, h, scale]);
  const a = new Int32Array(channel.arena);
  const was = [Atomics.load(a, M_OUT_W), Atomics.load(a, M_OUT_H)];
  Atomics.store(a, M_OUT_W, w | 0);
  Atomics.store(a, M_OUT_H, h | 0);
  if (scale) Atomics.store(a, M_SCALE, Math.max(1, scale | 0));
  // A resize, not the first size: every process with a block takes a turn,
  // whose pump tells its clients (`WlProxy.outputSize`). One whose threads
  // are all parked would otherwise hear of it only at its next wake.
  if (!was[0] || (was[0] === (w | 0) && was[1] === (h | 0))) return;
  for (let b = 0; b < BLOCKS; b++) {
    if (Atomics.load(a, blockBase(b) + B_TOKEN)) wakeBlock(channel, a, b);
  }
}

/**
 * Resolves when any process publishes after `seen` (the value of a previous
 * call, or undefined for "from now"), with the new value; null where there is
 * no `Atomics.waitAsync` (the caller keeps its frame clock alone).
 */
export async function waitPublish(channel, seen) {
  if (typeof Atomics.waitAsync !== 'function' || !isShared(channel.arena)) return null;
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
/// One scratch buffer for a record's text rather than one per surface per
/// frame: this runs for every surface at frame rate.
const textBytes = new Uint8Array(R_TITLE_WORDS * 4);
/** The UTF-8 text in `words` words at `at`: up to its first NUL. */
function recordText(a, at, words) {
  const bytes = textBytes;
  let end = words * 4;
  for (let w = 0; w < words; w++) {
    const v = a[at + w];
    if (v === 0) { end = w * 4; break; }
    bytes[w * 4] = v & 0xff;
    bytes[w * 4 + 1] = (v >>> 8) & 0xff;
    bytes[w * 4 + 2] = (v >>> 16) & 0xff;
    bytes[w * 4 + 3] = (v >>> 24) & 0xff;
  }
  const nul = bytes.subarray(0, end).indexOf(0);
  if (nul >= 0) end = nul;
  return end ? decoder.decode(bytes.subarray(0, end)) : '';
}

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
        // Each stops at its first NUL, or fills its field.
        title: recordText(a, r + R_TITLE, R_TITLE_WORDS),
        appId: recordText(a, r + R_APP_ID, R_APP_ID_WORDS),
        pid: a[r + R_PID] >>> 0,
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
  // On a mirror, the real channel's too (`mirrorChannel`).
  channel.mirror?.('releaseToken', [token]);
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
  // On a mirror, the real channel's too (`mirrorChannel`).
  channel.mirror?.('ackBlock', [block, gen]);
  const a = new Int32Array(channel.arena);
  Atomics.store(a, blockBase(block) + B_ACK, gen);
}

/** The global kernel's memory, for `sendEvent`'s wake. One per page. */
export function setKernelMemory(channel, memory) {
  if (memory && !channel.kernelMemory) channel.kernelMemory = memory;
}

/** The page's drag data: `text` (a text/uri-list) for the drag whose
 *  enter follows. False when it does not fit. */
export function setDragData(channel, text) {
  // On a mirror, the real channel's too (`mirrorChannel`).
  channel.mirror?.('setDragData', [text]);
  const b = new TextEncoder().encode(text);
  if (b.length > D_BYTES) return false;
  const a = new Int32Array(channel.arena);
  new Uint8Array(channel.arena, (dragBase + 1) * 4, D_BYTES).set(b);
  Atomics.store(a, dragBase, b.length);
  return true;
}

/** What block `block`'s guest last said of the page's drag:
 *  {serial, flags} (`DRAG_*`). */
export function dragState(channel, block) {
  const v = Atomics.load(new Int32Array(channel.arena), blockBase(block) + B_DRAG) >>> 0;
  return { serial: v >>> 4, flags: v & 15 };
}

// The host's clock object, looked up once in this realm: `performance` is a
// Blink attribute of the global scope, and reading it costs more than `now()`.
const perf = globalThis.performance;

/** Resolves with block `block`'s word for drag `serial` once `want(flags)`
 *  holds, or null after `ms`. */
export async function waitDrag(channel, block, serial, want, ms) {
  const a = new Int32Array(channel.arena);
  const at = blockBase(block) + B_DRAG;
  const end = perf.now() + ms;
  for (;;) {
    const v = Atomics.load(a, at);
    if (v >>> 4 === serial >>> 0 && want(v & 15)) return v & 15;
    const left = end - perf.now();
    if (left <= 0) return null;
    if (typeof Atomics.waitAsync === 'function' && isShared(channel.arena)) {
      const r = Atomics.waitAsync(a, at, v, Math.min(left, 100));
      if (r.async) await r.value;
    } else {
      await new Promise((res) => setTimeout(res, Math.min(left, 20)));
    }
  }
}

/** Write one event into a process's ring, for its guest to pick up. */
export function sendEvent(channel, block, kind, a1 = 0, b1 = 0, c1 = 0, d1 = 0, client = 0, surface = 0) {
  // On a mirror, the real channel's too (`mirrorChannel`).
  channel.mirror?.('sendEvent', [block, kind, a1, b1, c1, d1, client, surface]);
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
  wakeBlock(channel, a, block);
}

/** Wake the process that holds `block`, as an event for it would. */
export function wakeProcess(channel, block) {
  wakeBlock(channel, new Int32Array(channel.arena), block);
}

/** Notify the wait queue of the process that holds `block`. */
function wakeBlock(channel, a, block) {
  const q = Atomics.load(a, blockBase(block) + B_WAKE);
  const slots = Atomics.load(a, blockBase(block) + B_SLOTS);
  const k = channel.kernelMemory;
  if (q && slots && k) {
    notifyQueue(new Int32Array(k.buffer), q >>> 0, slots >>> 0);
    // The single-thread build: the fibers parked on those words look.
    if ((SP_CODE && single.on)) sched.checkSlots();
  } else if (q && slots && isShared(channel.arena)) {
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
  if (!kernel || channel.relay || typeof Atomics.waitAsync !== 'function' || !isShared(channel.arena)) return;
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
 *  (hosts/kernel/src/wait.rs `Queue`). */
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
      if (isShared(w.buffer)) Atomics.notify(w, at);
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
  // between two frames accumulate rather than the last one winning: client,
  // surface, x, y, w, h per entry, `#pendingN` of them. Typed arrays, as the
  // rest of a publish's scratch, so a publish makes no garbage.
  #pending = new Int32Array(SLOTS * 6);
  #pendingN = 0;
  /** The surfaces whose pixels this publish copies: their record's offset in
   *  the arena, the buffer they were last copied into, and the damage. */
  #shmDst = new Int32Array(SLOTS);
  #shmMine = new Uint32Array(SLOTS);
  #shmRect = new Int32Array(SLOTS * 4);
  /** A view of the guest's memory, made again when it grows. */
  #view = null;

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
  #copyPixels(memory, count) {
    if (!this.#pixels) return;
    const a = this.#a;
    // A buffer that cannot take this publish's new regions is replaced by
    // one that can, with room to spare, and every surface is copied whole.
    // A first one is made for what this publish needs.
    {
      const h = this.#buf && this.#bufHead();
      let more = 0;
      let all = 0;
      for (let k = 0; k < count; k++) {
        const dst = this.#shmDst[k];
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
          for (let k = 0; k < count; k++) {
            const dst = this.#shmDst[k];
            a[dst + R_ADDR] = 0;
            a[dst + R_ADDR_HI] = 0;
          }
          return;
        }
      }
    }
    const buf = this.#buf;
    const h = this.#bufHead();
    const find = (c, sf) => {
      for (let e = 0; e < SLOTS; e++) {
        const o = P_ENTRIES + e * PE;
        if (h[o + 2] && h[o] === c && h[o + 1] === sf) return o;
      }
      return -1;
    };
    while (Atomics.compareExchange(h, P_LOCK, 0, 1) !== 0);
    try {
      if (h[P_NEXT] < P_DATA) h[P_NEXT] = P_DATA;
      // The regions this publish needs; a buffer that cannot take the new
      // ones as well starts again from empty, every surface copied whole.
      let more = 0;
      for (let k = 0; k < count; k++) {
        const dst = this.#shmDst[k];
        const o = find(a[dst + R_CLIENT], a[dst + R_SURFACE]);
        const need = a[dst + R_WIDTH] * a[dst + R_HEIGHT] * 4;
        if (o < 0 || h[o + 3] < need) more += (need + 255) & ~255;
      }
      if (h[P_NEXT] + more > buf.byteLength) {
        for (let e = 0; e < SLOTS * PE; e++) h[P_ENTRIES + e] = 0;
        h[P_NEXT] = P_DATA;
      }
      const src = this.#bytesOf(memory);
      for (let k = 0; k < count; k++) {
        const dst = this.#shmDst[k];
        const mine = this.#shmMine[k] === this.#bufId;
        const rx = this.#shmRect[k * 4], ry = this.#shmRect[k * 4 + 1], rwd = this.#shmRect[k * 4 + 2], rht = this.#shmRect[k * 4 + 3];
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
          if (!whole && rwd > 0 && rht > 0) {
            x = Math.max(0, Math.min(rx, w));
            y = Math.max(0, Math.min(ry, ht));
            rw = Math.max(0, Math.min(rwd, w - x));
            rh = Math.max(0, Math.min(rht, ht - y));
          }
          const out = this.#bufBytes;
          if (from + (y + rh - 1) * stride + (x + rw) * 4 <= src.length || rh === 0) {
            // Whole rows at once where both sides are packed alike.
            if (rw === w && stride === w * 4) {
              const s0 = from + y * stride;
              out.set(src.subarray(s0, s0 + rh * stride), at + y * w * 4);
            } else {
              // A narrow damaged rectangle a word at a time: a view per row
              // was most of an idle-ish desktop's garbage.
              const aligned = ((from | stride) & 3) === 0;
              const s32 = aligned ? this.#words(memory) : null, o32 = this.#bufWords;
              for (let r = 0; r < rh; r++) {
                const s0 = from + (y + r) * stride + x * 4;
                const d0 = at + (y + r) * w * 4 + x * 4;
                if (rw >= 64 || !aligned) out.set(src.subarray(s0, s0 + rw * 4), d0);
                else for (let q = 0, sw = s0 >> 2, dw = d0 >> 2; q < rw; q++) o32[dw + q] = s32[sw + q];
              }
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

  /** The pixel buffer's header words, bytes and words, kept with the buffer. */
  #bufHeadView = null;
  #bufBytes = null;
  #bufWords = null;
  #bufHead() {
    if (this.#bufHeadView?.buffer !== this.#buf) {
      this.#bufHeadView = new Int32Array(this.#buf, 0, P_DATA >> 2);
      this.#bufBytes = new Uint8Array(this.#buf);
      this.#bufWords = new Int32Array(this.#buf);
    }
    return this.#bufHeadView;
  }

  /** `memory`'s bytes, a view kept while its buffer is the same. */
  #byteView = null;
  #bytesOf(memory) {
    const v = this.#byteView;
    return v && v.buffer === memory.buffer ? v : (this.#byteView = new Uint8Array(memory.buffer));
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
        Atomics.store(this.#a, base + B_DRAG, 0);
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

  /** This process's block of the arena, or -1. */
  get block() {
    return this.#block;
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
      this.#pendingN = 0;
    }
    if (!this.#ready()) return;
    const a = this.#a;
    const n = Math.min(count, SLOTS);
    const src = this.#words(memory);
    const gen = Atomics.load(a, this.#base + B_GEN);
    const drawn = Atomics.load(a, this.#base + B_ACK) === gen;
    if (drawn) this.#pendingN = 0;
    const pend = this.#pending;
    let shm = 0;
    for (let i = 0; i < n; i++) {
      const o = (ptr >>> 2) + i * REC;
      const dst = this.#base + B_WORDS + i * REC;
      const client = src[o + R_CLIENT], surface = src[o + R_SURFACE];
      // The buffer this slot's surface was last published from, if it is the
      // same surface.
      const same = a[dst + R_CLIENT] === client && a[dst + R_SURFACE] === surface;
      const mine = same && this.#pixels ? a[dst + R_ADDR_HI] >>> 0 : 0;
      for (let w = 0; w < REC; w++) a[dst + w] = src[o + w];
      let x = src[o + R_DAMAGE_X], y = src[o + R_DAMAGE_Y], dw = src[o + R_DAMAGE_W], dh = src[o + R_DAMAGE_H];
      if (this.#pixels && recordAddr(a, dst) && src[o + R_WIDTH] > 0 && src[o + R_HEIGHT] > 0) {
        this.#shmDst[shm] = dst;
        this.#shmMine[shm] = mine;
        this.#shmRect[shm * 4] = x;
        this.#shmRect[shm * 4 + 1] = y;
        this.#shmRect[shm * 4 + 2] = dw;
        this.#shmRect[shm * 4 + 3] = dh;
        shm++;
      }
      // Accumulate damage for a surface the compositor has not caught up
      // with (`mergeDamage`, w <= 0 meaning everything).
      let e = 0;
      while (e < this.#pendingN && !(pend[e * 6] === client && pend[e * 6 + 1] === surface)) e++;
      if (e < this.#pendingN) {
        const q = e * 6;
        if (pend[q + 4] <= 0 || pend[q + 5] <= 0 || dw <= 0 || dh <= 0) {
          x = 0; y = 0; dw = 0; dh = 0;
        } else {
          const r = Math.max(pend[q + 2] + pend[q + 4], x + dw), b = Math.max(pend[q + 3] + pend[q + 5], y + dh);
          x = Math.min(pend[q + 2], x);
          y = Math.min(pend[q + 3], y);
          dw = r - x;
          dh = b - y;
        }
      } else if (e < SLOTS) {
        this.#pendingN++;
      } else {
        // More surfaces undrawn than a block holds: all of this one.
        e = -1;
        x = 0; y = 0; dw = 0; dh = 0;
      }
      if (e >= 0) {
        const q = e * 6;
        pend[q] = client; pend[q + 1] = surface;
        pend[q + 2] = x; pend[q + 3] = y; pend[q + 4] = dw; pend[q + 5] = dh;
      }
      a[dst + R_DAMAGE_X] = x;
      a[dst + R_DAMAGE_Y] = y;
      a[dst + R_DAMAGE_W] = dw;
      a[dst + R_DAMAGE_H] = dh;
    }
    if (shm) this.#copyPixels(pixels, shm);
    Atomics.store(this.#a, this.#base + B_COUNT, n);
    Atomics.store(this.#a, this.#base + B_HEARTBEAT, Date.now());
    Atomics.add(this.#a, this.#base + B_GEN, 1);
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
    if (tail === head) return 0;
    const out = this.#words(memory);
    const at = ptr >>> 2;
    let count = 0;
    while (tail < head && count < max) {
      const slot = base + K_WORDS + (tail % RING) * EV;
      for (let w = 0; w < EV; w++) out[at + count * EV + w] = a[slot + w];
      tail++;
      count++;
    }
    Atomics.store(a, base + K_TAIL, tail);
    return count;
  }

  /** `memory`'s words, a view kept while its buffer is the same. */
  #words(memory) {
    const v = this.#view;
    return v && v.buffer === memory.buffer ? v : (this.#view = new Int32Array(memory.buffer));
  }

  #clipSeen = 0;

  /** The clipboard's number (`wl_clip_serial`). */
  clipSerial() {
    return (this.#clipSeen = Atomics.load(this.#a, clipBase + C_SERIAL) >>> 0);
  }

  /** The clipboard's text, up to `max` bytes into `ptr` of `memory`: its
   *  whole length (`wl_clip_get`). */
  clipGet(memory, ptr, max) {
    const a = this.#a;
    clipLock(a);
    const n = Math.min(Atomics.load(a, clipBase + C_LEN) >>> 0, C_BYTES);
    const k = Math.min(n, max);
    new Uint8Array(memory.buffer, ptr, k).set(new Uint8Array(a.buffer, (clipBase + C_HEAD) * 4, k));
    clipUnlock(a);
    return n;
  }

  /** A client's selection onto the clipboard (`wl_clip_set`, `from` 1 or
   *  2 as `clipPut`'s): its number, which this process has then seen. */
  clipSet(memory, ptr, len, from = 1) {
    return (this.#clipSeen = clipPut(this.#a, new Uint8Array(memory.buffer, ptr, len), from === 2 ? 2 : 1));
  }

  /** Who set the clipboard (`wl_clip_from`): 0 the page, 1 a guest client,
   *  2 a Wine client. */
  clipFrom() {
    return Atomics.load(this.#a, clipBase + C_FROM) >>> 0;
  }

  /** The page's drag data, up to `max` bytes into `ptr` of `memory`: its
   *  whole length. */
  dragData(memory, ptr, max) {
    const a = this.#a;
    const n = Math.min(Atomics.load(a, dragBase) >>> 0, D_BYTES);
    new Uint8Array(memory.buffer, ptr, Math.min(n, max)).set(new Uint8Array(a.buffer, (dragBase + 1) * 4, Math.min(n, max)));
    return n;
  }

  /** What the client says of the page's drag `serial` (`DRAG_*`), for the
   *  page (`dragState`, `waitDrag`). */
  dragStatus(serial, flags) {
    if (this.#block < 0) return;
    const at = this.#base + B_DRAG;
    Atomics.store(this.#a, at, ((serial << 4) | (flags & 15)) | 0);
    Atomics.notify(this.#a, at);
  }

  /** Whether this process's input ring holds an event. */
  inputPending() {
    // A resized output is work for the pump as well (`outputSize`), and a
    // new clipboard (`clipSerial`).
    if (this.#outSeen !== this.#outNow()) return true;
    if (this.#clipSeen !== (Atomics.load(this.#a, clipBase + C_SERIAL) >>> 0)) return true;
    if (this.#block < 0) return false;
    const a = this.#a;
    return Atomics.load(a, this.#ring + K_HEAD) !== Atomics.load(a, this.#ring + K_TAIL);
  }

  /**
   * The output's size now, as the compositor last set it (`setOutputSize`):
   * width in the high half, height in the low, for the shim's pump, which
   * tells its clients when it changes (`wl_output_size`).
   */
  outputSize() {
    this.#outSeen = this.#outNow();
    return this.#outSeen;
  }

  /** The size `outputSize` last gave the pump. */
  #outSeen = 0;

  #outNow() {
    const a = this.#a;
    return ((Atomics.load(a, M_OUT_W) & 0xffff) << 16 | (Atomics.load(a, M_OUT_H) & 0xffff)) >>> 0;
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

// -- the single-thread build's page: a mirror of the arena ------------------
//
// The arena and the guests' memories are in the system Worker, which the
// page's DOM desktop (page/src/dom-compositor.js) cannot read. The Worker
// sends the page what changed (`mirrorTake`): each block's header, a changed
// block's records, and the damaged rows of each surface's pixels, copied out
// of its process's memory into a buffer per surface, which the records then
// name as a channel with pixel buffers would (`R_ADDR_HI`). The page applies
// it to an arena of its own (`mirrorApply`), which the desktop reads; what
// the desktop writes (an event, an ack, the output's size) it writes there
// and sends to the Worker to write on the real one (`mirrorCall`).

/** Where a surface's pixels start in its buffer on the page. */
const MIRROR_AT = 64;

/** The page's channel: `post(fn, args)` sends a write to the Worker. */
export function mirrorChannel(post) {
  return { arena: new ArrayBuffer(TOTAL_WORDS * 4), pixels: true, mirror: post, buffers: new Map() };
}

/** A write the page made on its mirror, made on the real `channel`. */
export function mirrorCall(channel, fn, args) {
  const f = { setClipboard, setOutputSize, releaseToken, ackBlock, setDragData, sendEvent }[fn];
  if (f) f(channel, ...args);
}

/** What the Worker keeps between `mirrorTake`s. */
export function mirrorState() {
  return { gens: new Int32Array(BLOCKS * 3), surfaces: new Map(), nextId: 1, clip: -1, sent: 0 };
}

/**
 * What changed on `channel` since `st` last saw it, for the page: null when
 * nothing has. `memoryOf(token)` is a process's memory, as bytes.
 * `{ heads, blocks: [[b, words]], pixels: [[id, token, size, offset, bytes]],
 * clip }` and the buffers to transfer.
 */
export function mirrorTake(channel, st, memoryOf, force = false) {
  const a = new Int32Array(channel.arena);
  let changed = force;
  const heads = new Int32Array(M_WORDS + BLOCKS * B_WORDS);
  heads.set(a.subarray(0, M_WORDS));
  const blocks = [];
  const pixels = [];
  const transfer = [];
  const live = new Set();
  for (let b = 0; b < BLOCKS; b++) {
    const base = blockBase(b);
    heads.set(a.subarray(base, base + B_WORDS), M_WORDS + b * B_WORDS);
    const token = a[base + B_TOKEN];
    const gen = a[base + B_GEN];
    const count = token ? Math.min(a[base + B_COUNT], SLOTS) : 0;
    const was = b * 3;
    if (st.gens[was] === token && st.gens[was + 1] === gen && st.gens[was + 2] === count) {
      if (token) for (const [k] of st.surfaces) if (k.startsWith(`${b}:${token}:`)) live.add(k);
      continue;
    }
    st.gens[was] = token;
    st.gens[was + 1] = gen;
    st.gens[was + 2] = count;
    changed = true;
    if (!token) continue;
    const words = a.slice(base + B_WORDS, base + B_WORDS + count * REC);
    const mem = memoryOf(token);
    // A surface whose pixels could not be read yet (its process's memory
    // has not come): the block is looked at again next time.
    let unread = false;
    for (let i = 0; i < count; i++) {
      const r = i * REC;
      const key = `${b}:${token}:${words[r + R_CLIENT]}:${words[r + R_SURFACE]}`;
      live.add(key);
      const stride = words[r + R_STRIDE];
      const height = words[r + R_HEIGHT];
      const size = Math.max(0, stride * height);
      let s = st.surfaces.get(key);
      if (!s || s.size !== size) {
        s = { id: st.nextId++, size, serial: -1 };
        st.surfaces.set(key, s);
      }
      // The pixels the record names: its buffer on the page, past a header.
      // An address of 0 is a surface with no buffer (one the host's GL
      // draws, say); one that has a buffer is copied even when external,
      // since the page paints it under the GL frame.
      const addr = recordAddr(words, r);
      if (!addr || !size) {
        words[r + R_ADDR] = 0;
        words[r + R_ADDR_HI] = 0;
        continue;
      }
      words[r + R_ADDR] = MIRROR_AT;
      words[r + R_ADDR_HI] = s.id;
      if (!mem || addr + size > mem.length) {
        unread = true;
        continue;
      }
      const serial = words[r + R_SERIAL] >>> 0;
      if (serial === s.serial) continue;
      // The damaged rows: all of them the first time, after a commit missed,
      // or for one that named no damage (Xwayland's).
      const whole = s.serial === -1 || serial !== s.serial + 1 || words[r + R_DAMAGE_W] <= 0 || words[r + R_DAMAGE_H] <= 0;
      s.serial = serial;
      const y0 = whole ? 0 : Math.max(0, Math.min(height, words[r + R_DAMAGE_Y]));
      const y1 = whole ? height : Math.max(y0, Math.min(height, words[r + R_DAMAGE_Y] + words[r + R_DAMAGE_H]));
      if (y1 <= y0) continue;
      const bytes = mem.slice(addr + y0 * stride, addr + y1 * stride);
      pixels.push([s.id, token, MIRROR_AT + size, MIRROR_AT + y0 * stride, bytes]);
      transfer.push(bytes.buffer);
    }
    if (unread) st.gens[was + 1] = gen - 1;
    blocks.push([b, words]);
    transfer.push(words.buffer);
  }
  // A surface gone: its buffer on the page goes with it.
  const gone = [];
  for (const [k, s] of st.surfaces) {
    if (!live.has(k)) {
      st.surfaces.delete(k);
      gone.push(s.id);
    }
  }
  if (gone.length) changed = true;
  // The clipboard, when a guest has set it.
  let clip = null;
  const serial = a[clipBase + C_SERIAL];
  if (serial !== st.clip) {
    st.clip = serial;
    clip = a.slice(clipBase, clipBase + C_HEAD + Math.ceil(Math.min(a[clipBase + C_LEN] >>> 0, C_BYTES) / 4));
    transfer.push(clip.buffer);
    changed = true;
  }
  // The heads alone (heartbeats, acks, a drag's answer) once a second.
  const now = Date.now();
  if (!changed && now - st.sent < 1000) return null;
  st.sent = now;
  transfer.push(heads.buffer);
  return { update: { heads, blocks, pixels, gone, clip }, transfer };
}

/** The Worker's `mirrorTake`, on the page's `channel`; `pixelsTo(token, id,
 *  buffer, replaces)` hands a surface's new buffer to the desktop. */
export function mirrorApply(channel, { heads, blocks, pixels, gone, clip }, pixelsTo) {
  const a = new Int32Array(channel.arena);
  a.set(heads.subarray(0, M_WORDS), 0);
  for (let b = 0; b < BLOCKS; b++) a.set(heads.subarray(M_WORDS + b * B_WORDS, M_WORDS + (b + 1) * B_WORDS), blockBase(b));
  for (const [b, words] of blocks) a.set(words, blockBase(b) + B_WORDS);
  for (const id of gone) {
    const was = channel.buffers.get(id);
    channel.buffers.delete(id);
    if (was) pixelsTo(was.token, 0, null, id);
  }
  for (const [id, token, size, offset, bytes] of pixels) {
    let buf = channel.buffers.get(id);
    if (!buf) {
      buf = { token, view: new Uint8Array(size) };
      channel.buffers.set(id, buf);
      pixelsTo(token, id, buf.view.buffer, 0);
    }
    buf.view.set(bytes.subarray(0, Math.max(0, size - offset)), offset);
  }
  if (clip) a.set(clip, clipBase);
}
