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

  /**
   * Called when every guest thread is waiting for input. Resolve once some
   * has been delivered with `pushStdin`, or return false to say there will
   * never be any -- which is what a program reading to end-of-file needs.
   *
   * The default has no terminal behind it, so it says so straight away and a
   * read returns zero.
   */
  async readStdin() { return false; }
}

/**
 * The `gex.meta` custom section: the dispatch metadata a module carries so
 * that one file is enough to run a program. Walking the section headers is
 * cheap -- each is a byte and a length -- and stops at the first match.
 */
function metadataSection(wasm) {
  const dv = new DataView(wasm.buffer, wasm.byteOffset, wasm.byteLength);
  let i = 8;
  const leb = () => {
    let n = 0, shift = 0, b;
    do { b = dv.getUint8(i++); n |= (b & 0x7f) << shift; shift += 7; } while (b & 0x80);
    return n >>> 0;
  };
  while (i < wasm.length) {
    const id = dv.getUint8(i++);
    const size = leb();
    const end = i + size;
    if (id === 0) {
      const nameLen = leb();
      const name = new TextDecoder().decode(wasm.subarray(i, i + nameLen));
      if (name === 'gex.meta') return wasm.subarray(i + nameLen, end);
    }
    i = end;
  }
  return null;
}

export class Gex {
  #memory; #shim; #guest; #env; #u8; #imports; #net = null; #gl = null; #virgl = null; #virglTrace = false;

  constructor(env = new Env()) {
    this.#env = env;
    this.#memory = new WebAssembly.Memory({ initial: PAGES, maximum: PAGES });
  }

  get memory() { return this.#memory; }

  /** The shim instance, for diagnostics. */
  get shim() { return this.#shim; }

  /**
   * Read a file back out of the guest's filesystem, or null if it is not
   * there. The other half of bundling: a page that runs a compiler wants the
   * object file afterwards.
   */
  readFile(path) {
    const p = new TextEncoder().encode(path);
    const pp = this.#shim.exports.gex_alloc(p.length) >>> 0;
    this.#u8.set(p, pp);
    const len = this.#shim.exports.gex_get_file(pp, p.length) >>> 0;
    if (len === 0) return null;
    const at = this.#shim.exports.gex_file_data() >>> 0;
    return this.#u8.slice(at, at + len);
  }

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
        // The network. Every one of these must return without waiting: the
        // guest's thread is parked by the kernel, not by JS, because there is
        // one wasm stack and blocking here would take it.
        net_open: () => (this.#net ? this.#net.open() : 0),
        net_connect: (id, host, len, port) => {
          const name = new TextDecoder().decode(
            this.#u8.subarray(host >>> 0, (host >>> 0) + (len >>> 0)),
          );
          this.#net?.connect(id, name, port);
        },
        net_status: (id) => (this.#net ? this.#net.status(id) : 2),
        net_send: (id, ptr, len) =>
          this.#net
            ? this.#net.send(id, this.#u8.subarray(ptr >>> 0, (ptr >>> 0) + (len >>> 0)))
            : -1,
        net_recv: (id, ptr, len) =>
          this.#net
            ? this.#net.recv(id, this.#u8.subarray(ptr >>> 0, (ptr >>> 0) + (len >>> 0)))
            : 0,
        net_avail: (id) => (this.#net ? (this.#net.available(id) ? 1 : 0) : 1),
        net_close: (id) => this.#net?.close(id),
        // Graphics. The guest's GL library has already put a whole batch of
        // commands in its own memory; this hands over the address and the
        // page runs them on WebGL. Nothing is copied in either direction.
        gl_submit: (ring) => (this.#gl ? this.#gl.submit(this.#memory, ring >>> 0) : -1),
        // And the other way of giving the guest a GPU: unmodified Mesa in
        // the guest, speaking VirGL over a socket, decoded by virglrenderer
        // compiled to wasm. Bytes rather than an address, because that
        // renderer is a separate module with a heap of its own -- see
        // web/src/virgl.js.
        virgl_submit: (ptr, len) =>
          this.#virgl ? this.#virgl.submit(this.#memory, ptr >>> 0, len >>> 0) : -1,
        virgl_take: (ptr, max) =>
          this.#virgl ? this.#virgl.take(this.#memory, ptr >>> 0, max >>> 0) : 0,
      },
    });
    this.#u8 = new Uint8Array(this.#memory.buffer);
    // A shim older than this file still loads and still instantiates, and
    // then fails on the first call to something it does not have -- which
    // surfaces as `e.gex_paused_for_net is not a function` at the moment the
    // guest first waits for input, long after the mistake. Named here
    // instead, once, where it says what to do about it.
    const required = [
      'gex_alloc', 'gex_load', 'gex_add_file', 'gex_push_stdin', 'gex_wake',
      'gex_paused_for_input', 'gex_paused_for_net', 'gex_resume_pc',
      'gex_resume_ctx', 'gex_exit_code', 'gex_jit_pending_len',
    ];
    const missing = required.filter((n) => typeof this.#shim.exports[n] !== 'function');
    if (missing.length) {
      throw new Error(
        `the gex runtime is out of date: it has no ${missing.join(', ')}. ` +
          'Rebuild it with web/build-assets.sh and reload.',
      );
    }
    // Whatever `useGl` was told before the shim existed.
    this.#shim.exports.gex_set_gl?.(this.#gl ? 1 : 0);
    // Only when there really is one: a guest that connects to a socket
    // nobody serves is worse off than one that finds no socket at all,
    // because Mesa's vtest winsys does not fall back -- it writes its
    // handshake into the dead descriptor and blocks on a reply that cannot
    // come.
    this.#shim.exports.gex_set_virgl?.(this.#virgl ? (this.#virglTrace ? 2 : 1) : 0);
    // wasm-ld puts the shim's stack at address zero regardless of
    // --global-base, which is the guest's image. Move it into the shim's own
    // region, above everything the guest maps. Nothing has run yet.
    this.#shim.exports.__stack_pointer.value = SHIM_STACK_TOP | 0;
    if (opts.strace) this.#shim.exports.gex_set_trace(1);
    // A terminal changes how libc buffers and whether a shell prints a
    // prompt, so a page with xterm.js in it has to say so.
    if (opts.tty) this.#shim.exports.gex_set_tty(1);
    // A page has no user; an embedder that has one says so.
    if (opts.uid !== undefined) this.#shim.exports.gex_set_identity(opts.uid, opts.gid ?? opts.uid);

    // Hand the ELF across: the shim allocates inside its own region of the
    // shared memory, JS copies into it, and the shim does the rest -- the same
    // loader the native host uses, so a program is placed identically.
    // Coerced unsigned: the shim's own region sits above 2 GiB, and a wasm
    // i32 return arrives in JS signed.
    // The module describes its own dispatch, in a custom section. Runtime
    // translation needs it to carry on allocating table slots where the
    // ahead-of-time build stopped.
    const meta = metadataSection(guestWasm);
    if (meta && meta.length) {
      const mp = this.#shim.exports.gex_alloc(meta.length) >>> 0;
      this.#u8.set(meta, mp);
      this.#shim.exports.gex_set_metadata(mp, meta.length);
    }
    // Where the guest starts, for a program given relative paths.
    if (opts.cwd) {
      const c = new TextEncoder().encode(opts.cwd);
      const cp = this.#shim.exports.gex_alloc(c.length) >>> 0;
      this.#u8.set(c, cp);
      this.#shim.exports.gex_set_cwd(cp, c.length);
    }
    // Standard input, if the embedder has any: a page has nothing to block
    // on, so the guest is given all of it up front.
    if (opts.stdin && opts.stdin.length) {
      const sp = this.#shim.exports.gex_alloc(opts.stdin.length) >>> 0;
      this.#u8.set(opts.stdin, sp);
      this.#shim.exports.gex_set_stdin(sp, opts.stdin.length);
    }
    // The bundle goes over before the program does: a dynamic executable
    // names an interpreter, and the loader reads it out of this filesystem.
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


    const e = this.#shim.exports;
    this.#imports = {
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
    };
    this.#guest = await WebAssembly.instantiate(await WebAssembly.compile(guestWasm), {
      gex: this.#imports,
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
  /**
   * Run to completion and return the exit status.
   *
   * A loop, not a single call: a syscall that redirects execution -- a signal,
   * a `sigreturn` -- unwinds out of wasm and re-enters at a new address, and
   * so does a thread switch and a dispatch miss. That is the same shape the
   * native host uses and the reason guest stack depth never becomes wasm
   * stack depth.
   *
   * Asynchronous because of the miss: instantiating a module is, and a page
   * may not compile one synchronously anyway beyond a few kilobytes.
   */
  /** Hand the guest some standard input. Safe to call at any time. */
  pushStdin(bytes) {
    const p = this.#shim.exports.gex_alloc(bytes.length) >>> 0;
    this.#u8.set(bytes, p);
    this.#shim.exports.gex_push_stdin(p, bytes.length);
    this.#shim.exports.gex_free(p, bytes.length);
  }

  /** Say there will be no more input. */
  endStdin() { this.#shim.exports.gex_stdin_eof(); }

  /**
   * Give the guest a network.
   *
   * `net` is an object with the seven calls in `Net` below. Off by default:
   * a guest with no proxy behind it should see a machine with no network,
   * which fails cleanly, rather than one whose every connection hangs.
   */
  useNetwork(net) {
    this.#net = net;
    this.#shim.exports.gex_set_network(net ? 1 : 0);
  }

  /**
   * Give the guest a graphics device.
   *
   * `gl` is a `GlDevice` from `web/src/gl.js`, holding a canvas. Off by
   * default for the same reason the network is: a guest with no canvas behind
   * it should see a machine with no graphics device, which fails at `open`,
   * rather than one whose drawing disappears.
   */
  useGl(gl) {
    this.#gl = gl;
    // Before `load` is the useful order -- the device node is created while
    // the guest's filesystem is being built, and a guest that opens
    // /dev/gex-gl and does not find it concludes it has no graphics. Calling
    // this afterwards still works for a second run.
    this.#shim?.exports?.gex_set_gl?.(gl ? 1 : 0);
  }

  /**
   * Give the guest a virtio-gpu renderer instead.
   *
   * The other way of giving a guest a GPU, and the one that needs nothing of
   * the guest: `useGl` wants the program linked against gex's own libGLESv2,
   * while this wants Debian's Mesa and no changes at all. `renderer` is a
   * `VirglRenderer` from `web/src/virgl.js`.
   *
   * Before `load`, for a stronger reason than `useGl`: the socket is bound
   * while the guest's namespace is built, and Mesa's vtest winsys does not
   * fall back when it is missing -- it takes the ECONNREFUSED, writes its
   * handshake into the dead descriptor and blocks on a reply that cannot
   * come. A late call gives the next run a GPU and this one a hang.
   */
  useVirgl(renderer, trace = false) {
    this.#virgl = renderer;
    this.#virglTrace = trace;
    this.#shim?.exports?.gex_set_virgl?.(renderer ? (trace ? 2 : 1) : 0);
  }

  async run(entry) {
    // Every switch, signal and miss leaves wasm by trapping, and V8 captures
    // a stack trace for each trap it turns into a RuntimeError. Nothing here
    // ever reads one, and a program with contended locks takes hundreds of
    // thousands of them.
    const limit = Error.stackTraceLimit;
    if (!globalThis.__gexKeepTrace) Error.stackTraceLimit = 0;
    try {
      return await this.#runLoop(entry);
    } finally {
      Error.stackTraceLimit = limit;
    }
  }

  async #runLoop(entry) {
    let pc = entry;
    // The first guest thread's context; a switch hands back a different one.
    let ctx = 0x1000;
    for (;;) {
      try {
        this.#guest.exports.enter(ctx, pc);
        return this.#shim.exports.gex_exit_code();
      } catch (err) {
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
        const e = this.#shim.exports;
        if (e.gex_jit_pending_len() >>> 0) await this.#installTranslation();
        const next = e.gex_resume_pc();
        if (next !== 0n) {
          pc = next;
          const nextCtx = e.gex_resume_ctx() >>> 0;
          if (nextCtx !== 0) ctx = nextCtx;
          continue;
        }
        // The guest gave the wasm stack back without finishing, because
        // every thread is waiting for something only the page can produce.
        // Waiting has to happen out here: there is one wasm stack, and a
        // guest holding it is a guest nothing can be delivered to.
        //
        // The loop matters. One turn of the event loop is not a promise that
        // what is being waited for has arrived, and treating "still not
        // ready" as "finished" ended a program the moment its first `poll`
        // parked.
        if (e.gex_paused_for_net() || e.gex_paused_for_input()) {
          let woke = 0;
          while (e.gex_paused_for_net() || e.gex_paused_for_input()) {
            // Input first, and the two are not exclusive. A guest can have a
            // shell waiting on the keyboard and a download waiting on a
            // socket at the same time, and asking only about the network
            // meant a terminal that was never read from.
            if (e.gex_paused_for_input()) {
              // Waiting for a keystroke is an indefinite wait, and it should
              // be: no timer, no polling, the page simply idles. But when
              // something is waiting on the clock or a socket too, the wait
              // has to end on its own as well, or that other thread never
              // gets to run. Losing the race costs nothing -- a key that
              // arrives later is handed to the guest by the environment
              // whether or not anyone is still awaiting this promise.
              const key = this.#env.readStdin();
              const more = e.gex_paused_for_net()
                ? await Promise.race([key, new Promise((r) => setTimeout(() => r(true), 5))])
                : await key;
              if (more === false) {
                this.endStdin();
                // An environment with no input at all answers instantly, and
                // this loop would then be a spin that never lets anything
                // else happen. One tick is enough to keep the page alive.
                await new Promise((r) => setTimeout(r, 1));
              }
            } else {
              // A timer rather than a microtask: a microtask would run before
              // the socket callbacks this is waiting for.
              await new Promise((r) => setTimeout(r, 1));
            }
            woke = e.gex_wake();
            if (woke === 1) break;
          }
          if (woke !== 1) return e.gex_exit_code();
          pc = e.gex_resume_pc();
          const wakeCtx = e.gex_resume_ctx() >>> 0;
          if (wakeCtx !== 0) ctx = wakeCtx;
          continue;
        }
        if (String(err).includes("unreachable")) {
          return e.gex_exit_code();
        }
        throw err;
      }
    }
  }

  /**
   * Instantiate the module a dispatch miss left behind, and put its blocks in
   * the table the guest dispatches through.
   *
   * The shim lifted and encoded it; only the engine can compile it, and only
   * JS can ask the engine. The order matters: blocks go into the table first
   * and the shim publishes the page directory afterwards, so a branch into a
   * half-installed page keeps reaching the miss stub instead of a hole.
   */
  async #installTranslation() {
    const e = this.#shim.exports;
    const ptr = e.gex_jit_pending_ptr() >>> 0;
    const len = e.gex_jit_pending_len() >>> 0;
    const bytes = this.#u8.slice(ptr, ptr + len);
    const table = this.#guest.exports.dispatch;
    const need = e.gex_jit_slots() >>> 0;
    if (table.length < need) table.grow(need - table.length, table.get(0));
    const inst = await WebAssembly.instantiate(await WebAssembly.compile(bytes), {
      gex: { ...this.#imports, dispatch: table },
    });
    for (const name of Object.keys(inst.exports)) {
      if (name[0] !== 'b') continue;
      const slot = e.gex_dispatch_slot(BigInt('0x' + name.slice(1)));
      if (slot >= 0) table.set(slot >>> 0, inst.exports[name]);
    }
    e.gex_jit_installed();
  }

}
