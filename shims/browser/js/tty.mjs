// The page's end of the terminal.
//
// The terminal itself -- the flags, the size, the foreground process group,
// the line discipline -- is in the global kernel, in the memory every guest
// process shares. See `gex_kernel::tty` for why it has to be there. This is
// the other end of it: the thing holding an `xterm.js` calls `input` with what
// was typed and writes back whatever `drainEcho` returns.
//
// It reaches the kernel by *instantiating it*, which is the whole trick here.
// Everything else the page knows about a guest arrives as a message to a
// Worker, and a keystroke cannot: a `^C` has to interrupt a program that is
// not reading, may not be running, and may be one of a dozen Workers -- and
// the only thing that knows which processes are in the foreground group is
// the process table, which is in this module's memory. So the page makes its
// own instance and calls in directly. A keystroke becomes a signal in the
// same turn it was typed, and a Worker parked inside wasm is woken by the
// same counter every other cross-process delivery uses.
//
// Without a `SharedArrayBuffer` there is no global kernel, and there is
// nothing here: `open` answers null and the caller keeps its own line
// discipline, which is right for the one process it can then be running.

/// Where the page writes bytes for the kernel to copy out of.
///
/// The kernel imports a second memory -- the calling *process's* -- and
/// addresses it with `memory.copy` rather than a pointer. The page is not a
/// guest process and has none, so it provides a page of scratch and passes
/// offsets into it. One page: the largest thing that crosses is a 60-byte
/// `termios`, and keystrokes arrive a few at a time.
const SCRATCH = 0x100;
const SCRATCH_BYTES = 0x1000;

export class PageTty {
  #k;
  #mem;
  #scratch;

  /**
   * Instantiate the global kernel here, for the terminal's sake.
   *
   * `spec` is what `ProcessHost` was given: `{ module, memory }`. Null when
   * the page has no shared memory, and null out if the module is older than
   * this file -- an absent export is a rebuild, not a reason to fail a boot.
   */
  static async open(spec) {
    if (!spec?.module || !spec?.memory) return null;
    const mod = spec.module instanceof WebAssembly.Module
      ? spec.module
      : await WebAssembly.compile(spec.module);
    const scratch = new WebAssembly.Memory({ initial: 1 });
    let inst;
    try {
      inst = await WebAssembly.instantiate(mod, {
        env: { memory: spec.memory, process: scratch },
      });
    } catch {
      // A module that will not take a plain process memory is the shared
      // build, which is not what a page loads. Nothing here is worth failing
      // a boot for.
      return null;
    }
    const k = inst.exports;
    if (typeof k.k_tty_input !== 'function') return null;
    // A stack of this instance's own, before anything is called.
    //
    // Every instance of this module gets its own `__stack_pointer` global,
    // and wasm-ld initialises all of them to the same address in a memory
    // they share -- so two instances inside the kernel at once write over
    // each other's locals. The kernel reserves an array of slices for
    // exactly this.
    //
    // The *last* slice, and the arithmetic is the whole of why. `ProcessHost`
    // gives Worker n the value `area + n * size` as its stack *top*, counting
    // from 1, so that Worker owns the bytes below it -- `[area+(n-1)*size,
    // area+n*size)`. Taking `area + size` here reads like "slice 0, which
    // nobody has" and is in fact the exact top the first Worker is handed:
    // the page and that Worker then grow down through the same quarter
    // megabyte.
    //
    // It failed the way a stack collision does, rarely and somewhere else. A
    // forked child would trap with `memory access out of bounds` in the
    // middle of translating a block, but only when the fork landed close
    // enough to a keystroke for the two to be inside the kernel together.
    // Four runs of the same commands: two clean, two dead.
    //
    // `area + count * size` is above every top the host will ever hand out.
    //
    // Reading where the array is uses the linked-in stack, which is safe here
    // and nowhere else: the page instantiates before any Worker exists, so
    // there is nobody to collide with yet.
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
    // A memory that grew has a detached buffer behind it. This one never
    // grows, but reading it back costs nothing and a stale view is a silent
    // wrong answer rather than an error.
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
