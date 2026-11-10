// The host's GL frames of an X window, for a rootful X server.
//
// The page's GL device draws a GL client's frames, not the X server. With the
// compositor's own window manager (rootless) each X window is a surface of the
// page's and the scene draws the frame on it. Rootful there is one surface, the
// X screen, under the session's own window manager, so a frame must be in the
// window's own pixels for the server's stacking and clipping, and the manager's
// frames, to apply to it.
//
// So the compositor reads a presented frame back (`XGlReadback`, an async copy
// out of the GPU, started only when a new frame was presented and the window
// is viewable) and hands it to the X server through two shared buffers; the
// server puts it into the window between two turns of its loop
// (ports/xwayland/gexgl.c, hostx.js `xgl`).

import { wakeProcess } from '../../hosts/browser/js/wl-bridge.mjs';

/** Words of the control buffer. */
export const XGL = {
  /** EMPTY, READY (a frame waits for the server) or BUSY (one side has it). */
  STATE: 0,
  /** The waiting frame's window and size. */
  XID: 1,
  W: 2,
  H: 3,
  /** The window the compositor has frames for; the server says of it... */
  WATCH: 4,
  /** ...which window it looked at last, and whether that one is viewable
   *  (1), not (0), or not a window (-1). */
  SEEN: 5,
  VIEWABLE: 6,
  /** The last put's result (gexgl.c) and how many there were. */
  RESULT: 7,
  PUTS: 8,
  /** The server's arena block, for the wake. */
  BLOCK: 9,
  /** Microseconds the server spent taking and putting frames, summed. */
  SERVER_US: 10,
  WORDS: 16,
};
export const EMPTY = 0;
export const READY = 1;
export const BUSY = 2;

/** The two buffers, made by the X server's side and handed to the compositor. */
export function xglBuffers() {
  if (typeof SharedArrayBuffer !== 'function') return null;
  let pix;
  try {
    // Grown to the frame's size; 8K by 4K at most.
    pix = new SharedArrayBuffer(0, { maxByteLength: 8192 * 4096 * 4 });
  } catch {
    return null;
  }
  if (!pix.growable) return null;
  const ctl = new SharedArrayBuffer(XGL.WORDS * 4);
  Atomics.store(new Int32Array(ctl), XGL.BLOCK, -1);
  return { ctl, pix };
}

/**
 * The compositor's half: per displayed frame, `tick(device, frame, xid)`.
 * `frame` is the presented texture with a serial that moves on each present,
 * `xid` the X window it was presented for (or 0).
 */
export class XGlReadback {
  #ctl;
  #pix;
  #bytes;
  #channel;
  /** The serial of the frame last read back, for the window last watched. */
  #done = -1;
  #xid = 0;
  #viewable = 0;
  /** A copy out of the GPU in flight, and whether a newer frame came since. */
  #busy = false;
  /** A read-back frame the server has not taken yet: held until it has. */
  #held = null;
  #buffers = [];
  /** Copies handed over; ms summed: from a copy's submit to its buffer
   *  mapped, from there to the server's buffer being free, and writing the
   *  pixels into it; frames put by the server. */
  stats = { copies: 0, mapMs: 0, waitMs: 0, packMs: 0, puts: 0 };

  constructor({ ctl, pix }, channel) {
    this.#ctl = new Int32Array(ctl);
    this.#pix = pix;
    this.#channel = channel;
  }

  tick(device, frame, xid) {
    const c = this.#ctl;
    if (xid !== this.#xid) {
      this.#xid = xid;
      this.#done = -1;
    }
    Atomics.store(c, XGL.WATCH, xid);
    this.stats.puts = Atomics.load(c, XGL.PUTS);
    this.stats.serverMs = Atomics.load(c, XGL.SERVER_US) / 1000;
    if (this.#held) this.#hand();
    if (!xid || !frame) return;
    // Only into a window the server says is viewable: an unmapped one has no
    // pixels to put a frame in, and copying for it would be all cost.
    const viewable = Atomics.load(c, XGL.SEEN) === xid && Atomics.load(c, XGL.VIEWABLE) === 1;
    // Mapped again: its pixels are what the server painted since, so the
    // last frame goes back in even if no new one was presented.
    if (viewable && !this.#viewable) this.#done = -1;
    this.#viewable = viewable;
    if (!viewable || frame.serial === this.#done || this.#busy) return;
    this.#copy(device, frame.tex, frame.serial, xid);
  }

  async #copy(device, tex, serial, xid) {
    const w = tex.width;
    const h = tex.height;
    const stride = (w * 4 + 255) & ~255;
    const size = stride * h;
    let buf = this.#buffers.pop();
    if (buf && buf.size !== size) {
      buf.destroy();
      buf = null;
    }
    buf ??= device.createBuffer({ size, usage: GPUBufferUsage.COPY_DST | GPUBufferUsage.MAP_READ });
    this.#busy = true;
    this.#done = serial;
    const t0 = performance.now();
    const enc = device.createCommandEncoder();
    enc.copyTextureToBuffer({ texture: tex }, { buffer: buf, bytesPerRow: stride, rowsPerImage: h }, [w, h]);
    device.queue.submit([enc.finish()]);
    try {
      await buf.mapAsync(GPUMapMode.READ);
    } catch {
      this.#busy = false;
      buf.destroy();
      return;
    }
    const rgba = tex.format.startsWith('rgba');
    this.stats.format = tex.format;
    const t1 = performance.now();
    this.stats.mapMs += t1 - t0;
    // A newer frame than the one still held: that one is never handed over.
    if (this.#held) this.#drop(this.#held);
    this.#held = { buf, w, h, stride, xid, rgba, t1 };
    this.#busy = false;
    this.#hand();
  }

  /** A read-back buffer done with, for the next copy. */
  #drop(f) {
    f.buf.unmap();
    this.#buffers.push(f.buf);
  }

  /** The held frame to the server, if it has taken the last one. */
  #hand() {
    const c = this.#ctl;
    const f = this.#held;
    if (Atomics.compareExchange(c, XGL.STATE, EMPTY, BUSY) !== EMPTY
        && Atomics.compareExchange(c, XGL.STATE, READY, BUSY) !== READY) return;
    this.#held = null;
    const t2 = performance.now();
    const need = f.w * f.h * 4;
    if (this.#pix.byteLength < need) this.#pix.grow(need);
    if (!this.#bytes || this.#bytes.byteLength !== this.#pix.byteLength) this.#bytes = new Uint8Array(this.#pix);
    const range = f.buf.getMappedRange();
    const row = f.w * 4;
    if (!f.rgba) {
      const src = new Uint8Array(range);
      const out = this.#bytes;
      if (f.stride === row) out.set(src.subarray(0, need));
      else for (let y = 0; y < f.h; y++) out.set(src.subarray(y * f.stride, y * f.stride + row), y * row);
    } else {
      // The server's pixels are BGRA in memory: red and blue swapped, a
      // pixel a word.
      const src = new Uint32Array(range);
      const out = new Uint32Array(this.#pix, 0, f.w * f.h);
      const sw = f.stride >>> 2;
      for (let y = 0; y < f.h; y++) {
        let s = y * sw;
        let d = y * f.w;
        for (const e = d + f.w; d < e; d++, s++) {
          const v = src[s];
          out[d] = (v & 0xff00ff00) | ((v >>> 16) & 0xff) | ((v & 0xff) << 16);
        }
      }
    }
    this.#drop(f);
    Atomics.store(c, XGL.XID, f.xid);
    Atomics.store(c, XGL.W, f.w);
    Atomics.store(c, XGL.H, f.h);
    Atomics.store(c, XGL.STATE, READY);
    this.stats.copies++;
    this.stats.waitMs += t2 - f.t1;
    this.stats.packMs += performance.now() - t2;
    const block = Atomics.load(c, XGL.BLOCK);
    if (block >= 0) wakeProcess(this.#channel, block);
  }
}

/** The server's half, over its emscripten module `x`: what gexgl.c calls. */
export function xglServer({ ctl, pix }, x, block) {
  const c = new Int32Array(ctl);
  let bytes = null;
  let ptr = 0;
  let cap = 0;
  let t0 = 0;
  return {
    watch() {
      const b = block();
      if (b >= 0) Atomics.store(c, XGL.BLOCK, b);
      return Atomics.load(c, XGL.WATCH) >>> 0;
    },
    viewable(xid, v) {
      Atomics.store(c, XGL.VIEWABLE, v);
      Atomics.store(c, XGL.SEEN, xid | 0);
    },
    take(xidPtr, wPtr, hPtr) {
      if (Atomics.compareExchange(c, XGL.STATE, READY, BUSY) !== READY) return 0;
      t0 = performance.now();
      const w = Atomics.load(c, XGL.W);
      const h = Atomics.load(c, XGL.H);
      const need = w * h * 4;
      if (need > cap) {
        if (ptr) x._free(ptr);
        ptr = x._malloc(need);
        cap = ptr ? need : 0;
      }
      if (!ptr) {
        Atomics.store(c, XGL.STATE, EMPTY);
        return 0;
      }
      if (!bytes || bytes.byteLength !== pix.byteLength) bytes = new Uint8Array(pix);
      // Views made now: the server's memory may have grown in that malloc.
      const mem = x.wasmMemory.buffer;
      new Uint8Array(mem).set(bytes.subarray(0, need), ptr);
      const words = new Int32Array(mem);
      words[xidPtr >>> 2] = Atomics.load(c, XGL.XID);
      words[wPtr >>> 2] = w;
      words[hPtr >>> 2] = h;
      // Taken: the compositor may write the next one while this is put.
      Atomics.store(c, XGL.STATE, EMPTY);
      return ptr;
    },
    done(r) {
      Atomics.store(c, XGL.RESULT, r);
      if (r > 0) Atomics.add(c, XGL.PUTS, 1);
      Atomics.add(c, XGL.SERVER_US, Math.round((performance.now() - t0) * 1000));
    },
  };
}
