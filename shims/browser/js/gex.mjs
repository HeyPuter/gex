// The browser runtime: instantiate the shim and a guest module against one
// memory, and run.
//
// The shim is `gex-kernel` compiled to wasm. It imports the same memory the
// guest module does, so a syscall reads the guest's buffers where they lie --
// JS is not in the path of a `write`, only of the console it ends up on.
//
// Works unchanged in a browser; `run-node.mjs` beside it is the Node entry.

const GUEST_BYTES = 0xffff0000;
// The shim's world starts where the guest's ends (`STACK_TOP`); its stack sits
// at the bottom of that, below its data.
const SHIM_STACK_TOP = 0x80800000;
const PAGES = GUEST_BYTES / 65536;

/** Everything the host has to supply that wasm cannot do for itself. */
export class Env {
  /** Called with the bytes of a guest write to fd 1 or 2. */
  write(fd, bytes) {
    const text = new TextDecoder().decode(bytes);
    if (fd === 2) console.error(text);
    else console.log(text);
  }
  /** Nanoseconds since an arbitrary epoch. */
  now() { return BigInt(Math.round(performance.now() * 1e6)); }
}

export class Gex {
  #memory; #shim; #guest; #env; #u8;

  constructor(env = new Env()) {
    this.#env = env;
    this.#memory = new WebAssembly.Memory({ initial: PAGES, maximum: PAGES });
  }

  get memory() { return this.#memory; }

  /**
   * `shimWasm` is shims/browser's build output, `guestWasm` a module from
   * `gexc build --exhaustive --no-shared-memory`, `elf` the program's bytes.
   *
   * `files` is a map of guest path to contents. A browser has no filesystem to
   * pass through to, so whatever the program needs to read is bundled here.
   */
  async load(shimWasm, guestWasm, elf, files = {}, args = [], opts = {}) {
    const shimMod = await WebAssembly.compile(shimWasm);
    this.#shim = await WebAssembly.instantiate(shimMod, {
      env: { memory: this.#memory },
      // The shim asks JS for exactly the two things wasm has no way to do.
      gex_env: {
        write: (fd, ptr, len) =>
          this.#env.write(fd, this.#u8.subarray(ptr >>> 0, (ptr >>> 0) + (len >>> 0))),
        now: () => this.#env.now(),
      },
    });
    this.#u8 = new Uint8Array(this.#memory.buffer);
    // wasm-ld puts the shim's stack at address zero regardless of
    // --global-base, which is the guest's image. Move it into the shim's own
    // region, above everything the guest maps. Nothing has run yet.
    this.#shim.exports.__stack_pointer.value = SHIM_STACK_TOP | 0;
    if (opts.strace) this.#shim.exports.gex_set_trace(1);
    // A page has no user; an embedder that has one says so.
    if (opts.uid !== undefined) this.#shim.exports.gex_set_identity(opts.uid, opts.gid ?? opts.uid);

    // Hand the ELF across: the shim allocates inside its own region of the
    // shared memory, JS copies into it, and the shim does the rest -- the same
    // loader the native host uses, so a program is placed identically.
    // Coerced unsigned: the shim's own region sits above 2 GiB, and a wasm
    // i32 return arrives in JS signed.
    // Standard input, if the embedder has any: a page has nothing to block
    // on, so the guest is given all of it up front.
    if (opts.stdin && opts.stdin.length) {
      const sp = this.#shim.exports.gex_alloc(opts.stdin.length) >>> 0;
      this.#u8.set(opts.stdin, sp);
      this.#shim.exports.gex_set_stdin(sp, opts.stdin.length);
    }
    const ptr = this.#shim.exports.gex_alloc(elf.length) >>> 0;
    this.#u8.set(elf, ptr);
    const argvBlob = new TextEncoder().encode(args.join('\0'));
    let ap = 0;
    if (argvBlob.length) {
      ap = this.#shim.exports.gex_alloc(argvBlob.length) >>> 0;
      this.#u8.set(argvBlob, ap);
    }
    const entry = this.#shim.exports.gex_load(ptr, elf.length, ap, argvBlob.length);
    if (entry === 0n) throw new Error('gex: could not load the program');

    const enc = new TextEncoder();
    for (const [path, contents] of Object.entries(files)) {
      const p = enc.encode(path);
      const d = contents instanceof Uint8Array ? contents : enc.encode(contents);
      const pp = this.#shim.exports.gex_alloc(p.length) >>> 0;
      const dp = this.#shim.exports.gex_alloc(d.length) >>> 0;
      this.#u8.set(p, pp);
      this.#u8.set(d, dp);
      this.#shim.exports.gex_add_file(pp, p.length, dp, d.length);
    }

    const e = this.#shim.exports;
    this.#guest = await WebAssembly.instantiate(await WebAssembly.compile(guestWasm), {
      gex: {
        memory: this.#memory,
        syscall: e.gex_syscall,
        dispatch_miss: e.gex_dispatch_miss,
        illegal: e.gex_illegal,
        breakpoint: e.gex_breakpoint,
        sysreg_read: e.gex_sysreg_read,
        sysreg_write: e.gex_sysreg_write,
        cache_op: e.gex_cache_op,
        safepoint: e.gex_safepoint,
        fma: e.gex_fma,
      },
    });
    return entry;
  }

  /**
   * Run to completion and return the exit status.
   *
   * A loop, not a single call: a syscall that redirects execution -- a signal,
   * a `sigreturn` -- unwinds out of wasm and re-enters at a new address, which
   * is the same shape the native host uses and the reason guest stack depth
   * never becomes wasm stack depth.
   */
  run(entry) {
    // Every thread switch and every signal leaves wasm by trapping, and V8
    // captures a stack trace for each trap it turns into a RuntimeError.
    // Nothing here ever reads one, and a program with contended locks takes
    // hundreds of thousands of them.
    const limit = Error.stackTraceLimit;
    Error.stackTraceLimit = 0;
    try {
      return this.#runLoop(entry);
    } finally {
      Error.stackTraceLimit = limit;
    }
  }

  #runLoop(entry) {
    let pc = entry;
    // The first guest thread's context; a switch hands back a different one.
    let ctx = 0x1000;
    for (;;) {
      try {
        this.#guest.exports.enter(ctx, pc);
        return this.#shim.exports.gex_exit_code();
      } catch (err) {
        // The shim traps out of the guest stack rather than returning: a
        // redirect resumes elsewhere, anything else is the end of the run.
        // A thread switch is the same mechanism -- the shim leaves the stack
        // and names the context to come back on.
        // A trap unwinds the engine's own frames, but the shim's shadow
        // stack is a mutable global that Rust adjusts in each function's
        // prologue and restores in its epilogue -- and an abandoned frame
        // never runs its epilogue. Every trap therefore leaked a few dozen
        // bytes of it. Nothing was live below this point, so putting it back
        // is exact rather than approximate.
        //
        // Left to itself it slid about 32 bytes per trap, and a program that
        // switches threads often enough took it through 0x80000000 and into
        // the top of the guest's own stack, where it overwrote saved
        // registers: a thread would return through a link register holding
        // an address out of the shim's heap.
        this.#shim.exports.__stack_pointer.value = SHIM_STACK_TOP | 0;
        const next = this.#shim.exports.gex_resume_pc();
        if (next !== 0n) {
          pc = next;
          const nextCtx = this.#shim.exports.gex_resume_ctx() >>> 0;
          if (nextCtx !== 0) ctx = nextCtx;
          continue;
        }
        if (String(err).includes("unreachable")) {
          return this.#shim.exports.gex_exit_code();
        }
        throw err;
      }
    }
  }
}
