// The page's end of the terminal. The terminal itself -- flags, size,
// foreground group, line discipline -- lives in the global kernel's shared
// memory (see `gex_kernel::tty`); this holds an `xterm.js`, calls `input` with
// what was typed and writes back what `drainEcho` returns.
//
// It reaches the kernel by instantiating it directly, rather than messaging a
// Worker: a `^C` must interrupt a program that may not be reading or running
// and may be one of a dozen Workers, and only the process table (in this
// module's memory) knows the foreground group. So a keystroke becomes a signal
// in the same turn it was typed, waking a parked guest by the same counter
// every cross-process delivery uses.
//
// Without a `SharedArrayBuffer` there is no global kernel: `open` answers null
// and the caller keeps its own line discipline.

/// Where the page writes bytes for the kernel to copy out of. The kernel
/// imports the calling process's memory and addresses it with `memory.copy`;
/// the page has none, so it provides a page of scratch and passes offsets. One
/// page suffices -- the largest thing that crosses is a 60-byte `termios`.
const SCRATCH = 0x100;
const SCRATCH_BYTES = 0x1000;

export class PageTty {
  #k;
  #mem;
  #scratch;

  /**
   * Instantiate the global kernel here, for the terminal's sake. `spec` is
   * `{ module, memory }`; returns null when the page has no shared memory or
   * the module predates this file (an absent export is a rebuild, not a boot
   * failure).
   */
  static async open(spec) {
    if (!spec?.module || !spec?.memory) return null;
    const mod = spec.module instanceof WebAssembly.Module
      ? spec.module
      : await WebAssembly.compile(spec.module);
    // A maximum, because a memory import's limits are part of its type: one
    // with no maximum does not satisfy an import that declares one. 65535 is
    // what `tools/add-memory.mjs` writes for the process import.
    const scratch = new WebAssembly.Memory({ initial: 1, maximum: 65535 });
    let inst;
    try {
      inst = await WebAssembly.instantiate(mod, {
        env: { memory: spec.memory, process: scratch },
      });
    } catch (e) {
      // A module that will not take a plain process memory is the shared build,
      // not what a page loads. Warn rather than fail silently: the fallback
      // line editing looks almost right, but `^C` is quietly gone.
      console.warn(`[gex] no shared terminal: ${e.message}`);
      return null;
    }
    const k = inst.exports;
    if (typeof k.k_tty_input !== 'function') {
      console.warn('[gex] no shared terminal: the global kernel has no k_tty_input, '
        + 'so it is older than this glue. Rebuild it with shims/kernel/build.sh.');
      return null;
    }
    // A stack of this instance's own: every instance's `__stack_pointer` starts
    // at the same shared address, so two in the kernel at once overwrite each
    // other's locals. The kernel reserves an array of slices for this.
    //
    // `area + count * size` is above every top the host hands out. `area + size`
    // would collide with the first Worker (which is handed exactly that top),
    // and it failed as stack collisions do -- rarely, and somewhere else.
    // Reading where the array is uses the linked-in stack, safe only because
    // the page instantiates before any Worker exists.
    if (k.k_stack_area && inst.exports.__stack_pointer) {
      const area = k.k_stack_area() >>> 0;
      const size = k.k_stack_size() >>> 0;
      const count = k.k_stack_count() >>> 0;
      inst.exports.__stack_pointer.value = (area + count * size) | 0;
    }
    return new PageTty(k, scratch);
  }

  constructor(k, scratch) {
    this.#k = k;
    this.#scratch = scratch;
    this.#mem = new Uint8Array(scratch.buffer);
  }

  #bytes() {
    // A grown memory detaches its buffer. This one never grows, but re-reading
    // is free and a stale view is a silent wrong answer.
    if (this.#mem.buffer !== this.#scratch.buffer) {
      this.#mem = new Uint8Array(this.#scratch.buffer);
    }
    return this.#mem;
  }

  /**
   * Keystrokes, through the line discipline.
   *
   * Returns how many bytes of echo are queued. Everything else a key can mean
   * -- a line completed, a signal raised, the input flushed, end of input --
   * has already happened by the time this returns.
   */
  input(data) {
    const bytes = typeof data === 'string' ? new TextEncoder().encode(data) : data;
    let queued = 0;
    // In scratch-sized pieces. A paste is one `onData` of arbitrary length.
    for (let at = 0; at < bytes.length; at += SCRATCH_BYTES - SCRATCH) {
      const chunk = bytes.subarray(at, at + SCRATCH_BYTES - SCRATCH);
      this.#bytes().set(chunk, SCRATCH);
      queued = this.#k.k_tty_input(SCRATCH, chunk.length);
    }
    return queued;
  }

  /** What the terminal should display, as bytes. Empty when there is none. */
  drainEcho() {
    const out = [];
    for (;;) {
      const n = this.#k.k_tty_echo(SCRATCH, SCRATCH_BYTES - SCRATCH);
      if (n <= 0) break;
      out.push(this.#bytes().slice(SCRATCH, SCRATCH + n));
      if (n < SCRATCH_BYTES - SCRATCH) break;
    }
    if (!out.length) return null;
    if (out.length === 1) return out[0];
    const total = out.reduce((a, b) => a + b.length, 0);
    const all = new Uint8Array(total);
    let at = 0;
    for (const b of out) {
      all.set(b, at);
      at += b.length;
    }
    return all;
  }

  /**
   * Bytes a guest could read now: positive for waiting input, 0 for none,
   * -1 once there will be no more.
   */
  ready() {
    return this.#k.k_stdin_ready();
  }

  /** There will be no more input -- the page has gone, or `^D` at the end. */
  eof() {
    this.#k.k_stdin_eof();
    this.#k.k_wake?.();
  }

  /** `c_lflag`, which says whether the guest has taken over line editing. */
  lflag() {
    const n = this.#k.k_tty_get_termios(SCRATCH, 60);
    if (n < 16) return 0;
    const b = this.#bytes();
    return (b[SCRATCH + 12] | (b[SCRATCH + 13] << 8)
      | (b[SCRATCH + 14] << 16) | (b[SCRATCH + 15] << 24)) >>> 0;
  }

  /** The size the guest believes the terminal is, as `[rows, cols]`. */
  winsize() {
    const v = this.#k.k_tty_winsize();
    return [Number(v >> 48n) & 0xffff, Number((v >> 32n) & 0xffffn)];
  }

  /**
   * The terminal changed size. Signals `SIGWINCH` to the foreground group,
   * and only when it really changed -- a page that reports its size on every
   * animation frame would otherwise make `bash` redraw its prompt sixty times
   * a second.
   */
  resize(rows, cols) {
    const packed = (BigInt(rows & 0xffff) << 48n) | (BigInt(cols & 0xffff) << 32n);
    return this.#k.k_tty_resize(packed) !== 0;
  }

  /** The foreground process group, or 0 before a shell has claimed one. */
  fg() {
    return this.#k.k_tty_fg();
  }

  /** Raise a signal on the foreground group, for what no key stands for. */
  signal(signo) {
    return this.#k.k_tty_signal(signo >>> 0);
  }
}
