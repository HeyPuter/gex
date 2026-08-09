// The browser runtime: instantiate the shim and a guest module against one
// memory, and run.
//
// The shim is `gex-kernel` compiled to wasm. It imports the same memory the
// guest module does, so a syscall reads the guest's buffers where they lie --
// JS is not in the path of a `write`, only of the console it ends up on.
//
// Works unchanged in a browser; `run-node.mjs` beside it is the Node entry.

// How far a process's memory may grow, and what it is allocated with. The
// second is the number that matters: there is one memory per process, and it
// is paid for the moment the process exists. See
// `gex_abi::GUEST_MEM_INITIAL_BYTES`.
const GUEST_BYTES = 0xffff0000;
const GUEST_INITIAL_BYTES = 0x05000000;
// The shim's world starts where the guest's ends (`STACK_TOP`); its stack sits
// at the bottom of that, below its data.
const SHIM_STACK_TOP = 0x04800000;
// Everything below the guest's image: the reserved low region, the per-thread
// register contexts, and the dispatch directory. It is not in the memory map,
// so a forked child that copied only the mapped regions would arrive with no
// context to enter and no directory to dispatch through.
//
// `DISPATCH_DIR + DISPATCH_MAX_PAGES * 4`, from `gex_abi`.
const LOW_END = 0x00100000 + 0x80000 * 4;
const PAGES = GUEST_BYTES / 65536;
const INITIAL_PAGES = GUEST_INITIAL_BYTES / 65536;

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

  /**
   * Start a guest process the guest itself forked.
   *
   * `child` is `{ pid, pc, ctx, sp, tls, image }`, where `image` is the
   * parent's mapped memory as `{ at, bytes }` records. A host implements this
   * by making a `WebAssembly.Memory`, writing every record into it,
   * instantiating the shim and guest module against it, calling
   * `gex_adopt_child(pid, ctx, pc, sp, tls, brk)` and entering at what it
   * returns.
   *
   * There is no default, because the answer differs by host -- a page makes a
   * Worker, Node makes a `worker_thread` -- and because a silent default here
   * would mean a forking program quietly running only half of itself. A host
   * that does not implement it gets told the guest forked and it could not.
   */
  async spawnProcess(child) {
    throw new Error(
      `this environment cannot start guest process ${child.pid}: no spawnProcess`,
    );
  }

  /**
   * Start a guest thread the guest itself cloned.
   *
   * `thread` is `{ tid, ctx, pc, memory }`. A host implements this by making
   * a Worker, constructing a `Gex` on the *same* memory, and calling
   * `adoptThread`. Nothing is copied: that is the difference between this and
   * `spawnProcess`, and the reason a thread needs a shared memory and a
   * process does not.
   *
   * Declared here and deliberately not defined: a class *field* is set on
   * the instance at construction and would shadow a subclass's method of the
   * same name, which is how `WorkerEnv.spawnThread` came to be invisible to
   * the very code looking for it. `Gex` asks whether the method exists.
   */
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

// The dispatch tables a translated module imports and exports, in index
// order. Table 0 keeps the bare name it had when there was only one, so a
// module built before there were several still names the same thing.
//
// There is more than one because V8 caps a table at ten million entries,
// which at a slot per four-byte guest address is 38 MiB of text -- and the
// graphics rootfs is 89.5 MiB. See #64.
const TABLE_NAMES = [
  'dispatch',
  'dispatch1',
  'dispatch2',
  'dispatch3',
  'dispatch4',
  'dispatch5',
  'dispatch6',
  'dispatch7',
];

export class Gex {
  #memory; #shim; #guest; #env; #u8; #imports; #net = null; #gl = null; #virgl = null; #virglTrace = false;
  // The channel `#turn` posts to itself on, made once and kept.
  #tick = null;
  // How many of the process's translated modules *this* Worker has put into
  // its own dispatch tables. Per Worker, not per process: the tables belong to
  // a module instance and instances are not shared, so every thread starts
  // with empty tables however much the process has already translated.
  #installed = 0;
  // What `load` was told, so a forked child can be told the same. A child is
  // the same program with the same terminal and the same user; rebuilding its
  // kernel without them gave it a pipe where the parent had a tty, which is
  // enough to change how libc buffers and whether a shell prints a prompt.
  #opts = {};
  // Every module this process has put into its tables, compiled. Kept because
  // a forked child needs the same code in the same slots, and a
  // `WebAssembly.Module` crosses to a Worker without being compiled again --
  // which the bytes would not.
  #compiled = [];
  // `{ module, memory }` for the global kernel, and the instance made from it.
  //
  // The global kernel is a second wasm module holding the state that spans
  // *processes* -- the process table today, the file table and the pipes
  // next. Every Worker instantiates the same module against the same shared
  // memory, so its Rust data structures are one set of bytes for all of them.
  // Null is a run of one process, which needs none of that.
  #kernelSpec = null;
  #kernel = null;
  // This Worker's shim stack, claimed on the way in. See `#workerStack`.
  #stackTop = null;

  /**
   * `shared` asks for a SharedArrayBuffer, which needs the shim and guest
   * modules built for one -- a memory import's `shared` flag is part of its
   * type, so a plain module will not instantiate against it or the other way
   * round. It buys threads: a process gets one memory, and if that memory is
   * shared then each of its threads can be its own Worker and run at the same
   * time. Processes still never share memory with each other; that is what
   * keeps two of them from putting libraries at the same addresses.
   */
  /**
   * A millisecond that is actually a millisecond.
   *
   * `setTimeout(…, 1)` is not one. Once five timers nest, the HTML spec has
   * the browser clamp the delay to four milliseconds, and a park loop is
   * nothing but nested timers -- so every park cost four milliseconds no
   * matter how quickly the thing being waited for arrived. glxgears is the
   * case: a frame is a handful of parks, the client writing to the X server
   * and waiting, the server answering and waiting, and at four milliseconds
   * each that is a frame time of thirty-odd milliseconds. Which is exactly
   * the thirty frames a second the demo held whatever else changed --
   * nine times fewer pixels did not move it, and neither did halving the
   * round trips to the renderer, because neither was what it was spending.
   *
   * The nesting level comes from the task the timer is *set* in, and a
   * message is not a timer: going through a `MessageChannel` first puts the
   * level back to zero, so the millisecond asked for is the millisecond
   * given.
   *
   * Not a bare message task, tempting as it is. That spins: this loop would
   * turn as fast as the Worker can post to itself, and a page running four
   * guest processes has four Workers doing it, which starves the one thread
   * that serves the renderer. Measured, that is 0.4 frames a second -- far
   * worse than the clamp it removes.
   */
  /**
   * One turn of the event loop and nothing more.
   *
   * For after a `gex_park`, which has already done the waiting: what is
   * wanted here is only to let the messages that arrived while this Worker
   * was asleep be delivered. A message task rather than a timer because
   * there is nothing to wait for, and once per park is not a spin.
   */
  #turn() {
    if (!this.#tick) {
      this.#tick = new MessageChannel();
      this.#tick.port1.start();
    }
    return new Promise((r) => {
      this.#tick.port1.onmessage = () => r();
      this.#tick.port2.postMessage(0);
    });
  }

  #nap() {
    if (!this.#tick) {
      this.#tick = new MessageChannel();
      this.#tick.port1.start();
    }
    return new Promise((r) => {
      this.#tick.port1.onmessage = () => setTimeout(r, 1);
      this.#tick.port2.postMessage(0);
    });
  }

  constructor(env = new Env(), { shared = false, memory = null, kernel = null } = {}) {
    this.#env = env;
    // `memory` adopts a process's existing address space rather than making
    // one. That is what a *thread* is here: a second Worker on the memory a
    // process already has, seeing the same guest and -- because the shim's own
    // state lives in that memory too -- the same kernel, the same file table,
    // the same everything. A new memory would be a new process.
    this.#memory =
      memory ??
      new WebAssembly.Memory({ initial: INITIAL_PAGES, maximum: PAGES, shared });
    this.#kernelSpec = kernel;
  }

  /**
   * Where this Worker's shim stack goes.
   *
   * A wasm global belongs to an instance, so every Worker has a stack pointer
   * of its own -- and pointing all of them at the same address is fine right
   * up until two Workers share a memory, which is what a thread is. The
   * shared build hands out a stack and a thread-local block per Worker;
   * `gex_worker_init` claims a pair and answers with the stack.
   *
   * The plain build has no such export and needs none: one Worker, one
   * memory, one stack.
   */
  #workerStack() {
    // Claimed once and remembered. This is asked again after every trap --
    // the stack pointer has to go back to the top, because a trap abandons
    // the stack rather than unwinding it -- and claiming a fresh slot each
    // time would exhaust sixty-four of them in the first second of a run.
    if (this.#stackTop === null) {
      const at = this.#shim.exports.gex_worker_init?.();
      this.#stackTop = at ? at | 0 : SHIM_STACK_TOP | 0;
    }
    return this.#stackTop;
  }

  /**
   * The guest's memory as bytes, re-made when it has grown.
   *
   * A memory that grows *detaches* the `ArrayBuffer` behind it, and every
   * view onto the old one stops working -- `Cannot perform
   * %TypedArray%.prototype.set on a detached ArrayBuffer`. That is not an
   * error case here: a process starts with the few tens of megabytes its
   * layout needs and grows as the guest maps things, so this happens on any
   * run that does real work.
   */
  /**
   * Make this process's memory cover `end`, growing it if it does not.
   *
   * A process starts with the fixed layout its ABI needs and grows as it
   * maps; the shim does that for itself from inside, and this is for the
   * places JavaScript writes guest memory directly -- which is a forked
   * child's image and nothing else.
   */
  #growTo(end) {
    const have = this.#memory.buffer.byteLength;
    if (end <= have) return;
    this.#memory.grow(Math.ceil((end - have) / 65536));
    this.#u8 = null;
  }

  #bytes() {
    if (!this.#u8 || this.#u8.byteLength === 0) {
      this.#u8 = new Uint8Array(this.#memory.buffer);
    }
    return this.#u8;
  }

  get memory() { return this.#memory; }

  /** The shim instance, for diagnostics. */
  get shim() { return this.#shim; }

  /**
   * How many of the process's translated modules this Worker has put into its
   * own dispatch tables.
   *
   * A thread that joins late must reach the same number as the process has
   * produced, or its tables are missing code the shared page directory
   * already points at.
   */
  get installedModules() { return this.#installed; }

  /**
   * Read a file back out of the guest's filesystem, or null if it is not
   * there. The other half of bundling: a page that runs a compiler wants the
   * object file afterwards.
   */
  readFile(path) {
    const p = new TextEncoder().encode(path);
    const pp = this.#shim.exports.gex_alloc(p.length) >>> 0;
    this.#bytes().set(p, pp);
    const len = this.#shim.exports.gex_get_file(pp, p.length) >>> 0;
    if (len === 0) return null;
    const at = this.#shim.exports.gex_file_data() >>> 0;
    return this.#bytes().slice(at, at + len);
  }


  /**
   * Everything the shim asks of JS: the console, the clock, the network and
   * the two graphics paths.
   *
   * A method rather than an inline object because `attach` needs the same set
   * -- a thread's Worker instantiates the same shim and must supply the same
   * imports, or it will not link.
   */
  #hostImports() {
    return {
        write: (fd, ptr, len) =>
          this.#env.write(fd, this.#bytes().subarray(ptr >>> 0, (ptr >>> 0) + (len >>> 0))),
        now: () => this.#env.now(),
        // The network. Every one of these must return without waiting: the
        // guest's thread is parked by the kernel, not by JS, because there is
        // one wasm stack and blocking here would take it.
        net_open: () => (this.#net ? this.#net.open() : 0),
        net_connect: (id, host, len, port) => {
          const name = new TextDecoder().decode(
            this.#bytes().subarray(host >>> 0, (host >>> 0) + (len >>> 0)),
          );
          this.#net?.connect(id, name, port);
        },
        net_status: (id) => (this.#net ? this.#net.status(id) : 2),
        net_send: (id, ptr, len) =>
          this.#net
            ? this.#net.send(id, this.#bytes().subarray(ptr >>> 0, (ptr >>> 0) + (len >>> 0)))
            : -1,
        net_recv: (id, ptr, len) =>
          this.#net
            ? this.#net.recv(id, this.#bytes().subarray(ptr >>> 0, (ptr >>> 0) + (len >>> 0)))
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
        // Every GL program that opens the socket is its own vtest client with
        // its own context and its own stream, so which one these bytes belong
        // to has to be said.
        virgl_submit: (session, ptr, len) =>
          this.#virgl
            ? this.#virgl.submit(session >>> 0, this.#memory, ptr >>> 0, len >>> 0)
            : -1,
        virgl_take: (session, ptr, max) =>
          this.#virgl
            ? this.#virgl.take(session >>> 0, this.#memory, ptr >>> 0, max >>> 0)
            : 0,
        virgl_close: (session) => this.#virgl?.close(session >>> 0),
    };
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
    await this.#startKernel();
    this.#shim = await WebAssembly.instantiate(shimMod, {
      env: { memory: this.#memory },
      // The shim asks JS for exactly the two things wasm has no way to do.
      gex_env: this.#hostImports(),
      gex_k: this.#kernelImports(),
    });
    this.#u8 = null;
    // First, before any call into the shim.
    //
    // `gex_worker_init` is what gives this Worker its own thread-local block,
    // and until it has run `__tls_base` is zero -- so a shim function that
    // touches a thread-local before it writes through a null base, into the
    // bottom of the guest's address space, which is where the register
    // contexts live. Every call below is such a function.
    this.#shim.exports.__stack_pointer.value = this.#workerStack();
    // A shim older than this file still loads and still instantiates, and
    // then fails on the first call to something it does not have -- which
    // surfaces as `e.gex_paused_for_net is not a function` at the moment the
    // guest first waits for input, long after the mistake. Named here
    // instead, once, where it says what to do about it.
    const required = [
      'gex_alloc', 'gex_load', 'gex_add_file', 'gex_push_stdin', 'gex_wake',
      'gex_paused_for_input', 'gex_paused_for_net', 'gex_paused_for_host',
      'gex_park', 'gex_resume_pc',
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
    this.#shim.exports.__stack_pointer.value = this.#workerStack();
    this.#opts = { strace: opts.strace, tty: opts.tty, uid: opts.uid, gid: opts.gid };
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
    this.#setMetadata(guestWasm);
    // Where the guest starts, for a program given relative paths.
    if (opts.cwd) {
      const c = new TextEncoder().encode(opts.cwd);
      const cp = this.#shim.exports.gex_alloc(c.length) >>> 0;
      this.#bytes().set(c, cp);
      this.#shim.exports.gex_set_cwd(cp, c.length);
    }
    // Standard input, if the embedder has any: a page has nothing to block
    // on, so the guest is given all of it up front.
    if (opts.stdin && opts.stdin.length) {
      const sp = this.#shim.exports.gex_alloc(opts.stdin.length) >>> 0;
      this.#bytes().set(opts.stdin, sp);
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
      this.#bytes().set(p, pp);
      this.#bytes().set(d, dp);
      this.#shim.exports.gex_add_file(pp, p.length, dp, d.length);
    }

    const ptr = this.#shim.exports.gex_alloc(elf.length) >>> 0;
    this.#bytes().set(elf, ptr);
    const argvBlob = new TextEncoder().encode(args.join('\0'));
    let ap = 0;
    if (argvBlob.length) {
      ap = this.#shim.exports.gex_alloc(argvBlob.length) >>> 0;
      this.#bytes().set(argvBlob, ap);
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
   * Join a process that is already loaded, as one of its threads.
   *
   * The counterpart of `load`: same memory, same shim, same guest module, but
   * no program is placed and no kernel is built. Both already exist, in the
   * memory this Worker just adopted -- the shim's heap and its statics live
   * there, so the `Host` that owns the file table is simply *there* to find.
   *
   * Instantiating a second time is safe, and only because of how the shared
   * build is linked: `--shared-memory` makes wasm-ld emit passive data
   * segments and an `__wasm_init_memory` guarded by an atomic, so the first
   * instance initialises the module's data and later ones leave it alone.
   * Against a plain memory those segments are active and a second
   * instantiation would quietly wipe the first thread's kernel.
   */
  async attach(shimWasm, guestWasm, { devices = true } = {}) {
    await this.#startKernel();
    this.#shim = await WebAssembly.instantiate(await WebAssembly.compile(shimWasm), {
      env: { memory: this.#memory },
      gex_env: this.#hostImports(),
      gex_k: this.#kernelImports(),
    });
    this.#u8 = null;
    // The shim's stack pointer is a mutable *global*, which belongs to the
    // instance rather than to the memory -- so every Worker has its own and
    // every Worker has to be told where it goes. Two threads sharing one stack
    // is the thing this design exists to avoid.
    this.#shim.exports.__stack_pointer.value = this.#workerStack();
    // The devices this process has, which `load` sets from the same fields.
    // Before the kernel is built, because that is when the device nodes are
    // made and the vtest socket is bound: a forked child told about its GPU
    // afterwards would have a `/dev` without one, and Mesa would find no
    // socket to draw through.
    // A *process* says what devices it has; a thread says nothing.
    //
    // These are process-wide flags in a memory the threads share, so a thread
    // that answered for itself would answer for everybody: a Worker joining a
    // process with a GPU and a network, having been given neither, turned
    // both off underneath the threads that were using them.
    if (devices) {
      this.#shim.exports.gex_set_gl?.(this.#gl ? 1 : 0);
      this.#shim.exports.gex_set_virgl?.(this.#virgl ? (this.#virglTrace ? 2 : 1) : 0);
      this.#shim.exports.gex_set_network?.(this.#net ? 1 : 0);
    }
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
    // Everything the process translated before this thread existed.
    await this.#catchUp();
  }

  /**
   * Put any modules this Worker is missing into its own dispatch tables.
   *
   * The page directory is in shared memory, so a joining thread's guest code
   * is already being sent to slots -- slots that this Worker's tables have
   * never had filled, because a table belongs to an instance. Without this a
   * thread's first indirect branch reaches whatever the table holds, which is
   * the miss stub at best and nothing at all past the table's end.
   *
   * In order, and from the start: each module's page assignments are the next
   * one's starting point, so they cannot be applied out of sequence.
   */
  /**
   * Everything this process has mapped, as `{ at, bytes }`.
   *
   * What a forked child needs and the only thing it needs: its image, its
   * stack and its heap, which after `fork` are exactly the parent's. The low
   * region comes too -- the register contexts and the dispatch directory live
   * below the image and are not in the memory map, and a child with neither
   * has no context to enter and no directory to dispatch through.
   *
   * Copied rather than shared, deliberately. Two processes sharing a memory
   * is the thing the switcher was built to cope with; this is a copy once, at
   * `fork`, instead of a copy on every switch for the life of both.
   */
  #processImage() {
    const e = this.#shim.exports;
    const out = [{ at: 0, bytes: this.#bytes().slice(0, LOW_END) }];
    const scratch = e.gex_alloc(24) >>> 0;
    const dv = new DataView(this.#memory.buffer);
    const n = e.gex_vma_count();
    for (let i = 0; i < n; i++) {
      if (e.gex_vma_at(i, scratch) !== 0) continue;
      const start = Number(dv.getBigUint64(scratch, true));
      const end = Number(dv.getBigUint64(scratch + 8, true));
      const prot = Number(dv.getBigUint64(scratch + 16, true));
      // `prot` travels with the bytes: the child rebuilds its memory *map*
      // from these, and a map is not recoverable from the memory it
      // describes. A child whose map is empty maps straight over its image.
      if (end > start) out.push({ at: start, end, prot, bytes: this.#bytes().slice(start, end) });
    }
    e.gex_free?.(scratch, 24);
    return out;
  }

  /**
   * Instantiate the global kernel against this process's memory.
   *
   * Every Worker does this, with the same module and the same *kernel*
   * memory, and a different process memory. The kernel memory is where the
   * module's own Rust state lives, so all of them see one process table; the
   * process memory is memory 1, which is how it copies bytes into and out of
   * a process it cannot address with a pointer.
   *
   * Nothing here without a `kernel` in the constructor's options: a run of one
   * process needs no table spanning processes, and paying 4.8 MB of module
   * for one would be silly.
   */
  async #startKernel() {
    if (!this.#kernelSpec || this.#kernel) return;
    const { module, memory, stackTop } = this.#kernelSpec;
    const mod = module instanceof WebAssembly.Module
      ? module
      : await WebAssembly.compile(module);
    const inst = await WebAssembly.instantiate(mod, {
      env: { memory, process: this.#memory },
    });
    // The stack, before anything is called.
    //
    // A wasm global belongs to an instance and every Worker makes its own, so
    // each has a `__stack_pointer` of its own -- and wasm-ld initialises
    // every one of them to the same address in a memory they all share. Two
    // Workers inside the kernel at once therefore write their locals over
    // each other. It showed up as an unaligned atomic, as a hang, and as a
    // fork that passed whenever tracing slowed the child enough to miss the
    // overlap.
    //
    // `stackTop` is a slice of the array the kernel reserves for exactly
    // this. The first Worker has no `stackTop` and keeps the stack the module
    // was linked with; it is also the one that reads the numbers out, because
    // it is the only one that can safely make a call at all.
    if (stackTop) inst.exports.__stack_pointer.value = stackTop | 0;
    this.#kernel = inst.exports;
  }

  /**
   * Where the global kernel's per-Worker stacks are, for the host to hand
   * out. Null when there is no global kernel or it is too old to say.
   *
   * Read from this Worker, which must be the first: see `#startKernel`.
   */
  kernelStacks() {
    const k = this.#kernel;
    if (!k?.k_stack_area) return null;
    return { area: k.k_stack_area() >>> 0, size: k.k_stack_size() >>> 0, count: k.k_stack_count() >>> 0 };
  }

  /** The global kernel's exports, or null. */
  get kernel() { return this.#kernel; }

  /**
   * What the shim calls to reach the process table every process shares.
   *
   * `k_present` is the whole of the negotiation: nought means there is no
   * global kernel, and the shim keeps the table in its own memory and behaves
   * as it did before any of this existed. So the stubs below are never
   * called -- they exist because a wasm import cannot be absent, only unused.
   */
  #kernelImports() {
    const k = this.#kernel;
    const absent = () => 0;
    // Named once, so a call added on one side and forgotten on the other is
    // a link error at instantiation rather than a stub that quietly answers
    // nought for the life of the run.
    const NAMES = [
      'k_spawn', 'k_exit', 'k_reap', 'k_wait', 'k_wait_ready',
      'k_has_children', 'k_kill', 'k_take_signals', 'k_has_signals',
      'k_ppid', 'k_pgid', 'k_sid', 'k_setsid', 'k_setpgid',
      'k_request_halt', 'k_halt_code',
      'k_fs_open', 'k_fs_close', 'k_fs_read', 'k_fs_write', 'k_fs_meta',
      'k_fs_truncate', 'k_fs_read_dir', 'k_fs_path_meta', 'k_fs_read_link',
      'k_fs_unlink', 'k_fs_mkdir', 'k_fs_rmdir', 'k_fs_rename', 'k_fs_chmod',
      'k_fs_symlink', 'k_fs_link', 'k_fs_mksocket', 'k_fs_add', 'k_fs_populated',
      'k_fs_populate_done',
      'k_pipe_new', 'k_pipe_open_end', 'k_pipe_close_end', 'k_pipe_read',
      'k_pipe_write', 'k_pipe_readable', 'k_pipe_writable', 'k_pipe_buffered',
      'k_sock_bind', 'k_sock_connect', 'k_sock_accept', 'k_sock_pending',
      'k_sock_read', 'k_sock_write', 'k_sock_readable', 'k_sock_writable',
      'k_sock_available', 'k_sock_close', 'k_sock_shutdown', 'k_sock_exists',
      'k_virgl_take', 'k_virgl_release',
      'k_wake_seq', 'k_wake', 'k_park',
    ];
    // Not imported by the shim -- the host reads it after a crash -- so it is
    // not in the list above and its absence is not a link error.
    // Two of these return an i64, which JS has to answer with a BigInt.
    const BIG = new Set(['k_wait', 'k_take_signals']);
    const out = { k_present: () => (k ? 1 : 0) };
    for (const n of NAMES) {
      if (k) {
        if (typeof k[n] !== 'function') {
          throw new Error(
            `the global kernel has no ${n}: it is older than this glue. ` +
              'Rebuild it with shims/kernel/build.sh.',
          );
        }
        out[n] = k[n];
      } else {
        out[n] = BIG.has(n) ? () => 0n : absent;
      }
    }
    // Nobody has asked for a halt, rather than "halt with status 0".
    if (!k) out.k_halt_code = () => -1;
    return out;
  }

  /**
   * Hand the shim the module's `gex.meta` section.
   *
   * Runtime translation carries on from where the ahead-of-time build
   * stopped, and this is the record of where that was. A kernel without it
   * reports "nothing to translate" at the first address the tables do not
   * already cover -- which for a forked child is the first address its
   * *parent* had not reached either.
   */
  #setMetadata(guestWasm) {
    const meta = metadataSection(guestWasm);
    if (!meta || !meta.length) return;
    const mp = this.#shim.exports.gex_alloc(meta.length) >>> 0;
    this.#bytes().set(meta, mp);
    this.#shim.exports.gex_set_metadata(mp, meta.length);
  }

  /**
   * Which dispatch slots this process's translated pages are in.
   *
   * Read out as one block rather than page by page: the table is a sorted
   * map, and a real process has tens of thousands of pages in it.
   */
  #pageMap() {
    const e = this.#shim.exports;
    if (!e.gex_page_count) return [];
    const n = e.gex_page_count() >>> 0;
    if (!n) return [];
    const p = e.gex_alloc(n * 12) >>> 0;
    const wrote = e.gex_page_dump(p, n * 12);
    const out = wrote > 0 ? this.#bytes().slice(p, p + wrote * 12) : new Uint8Array(0);
    e.gex_free?.(p, n * 12);
    return out;
  }

  /**
   * Become the child a `fork` in another Worker produced.
   *
   * Everything a process is, in the order the pieces depend on each other:
   *
   *   * its memory -- the parent's mapped regions, copied,
   *   * a shim and a guest module instantiated against that memory,
   *   * a kernel: its pid, its break, its memory map,
   *   * its dispatch slots, and the code already in them.
   *
   * The last is the one that is easy to miss and impossible to work around
   * afterwards. The child inherits the parent's dispatch *directory* with the
   * memory, so its guest code branches straight into slots the parent
   * allocated -- and a child that had not adopted the same map and the same
   * modules would find those slots empty, or past the end of a table it had
   * never grown.
   */
  async adoptProcess(shimWasm, guestWasm, child) {
    // Narrated when asked, because every step here is a place a child can
    // stop dead with nothing to show for it: a copy, two compiles, a kernel
    // and a table of code. A child that hangs is indistinguishable from one
    // that is merely slow to start, and it took a bisect to find out which.
    const step = globalThis.GEX_DEBUG_EXIT
      ? (s) => console.error(`[gex] adopt ${child.pid}: ${s}`)
      : () => {};
    step('copying the image');
    // The parent's memory had grown to hold what it mapped; this one has
    // only what a process starts with, so it has to catch up before a byte
    // is written. Without this the copy throws `RangeError: offset is out of
    // bounds` on the first region above the initial size.
    this.#growTo(child.image.reduce((n, r) => Math.max(n, r.at + r.bytes.byteLength), 0));
    const u8 = this.#bytes();
    for (const r of child.image) u8.set(new Uint8Array(r.bytes), r.at);
    // How much of the context survives each step. A child that enters at zero
    // arrived with a context of zeros, and the only question worth asking is
    // which of the three steps below emptied it.
    const live = () => {
      let n = 0;
      const at = Number(child.ctx);
      for (let i = at; i < at + 512; i++) if (u8[i]) n++;
      return n;
    };
    step(`image copied, ${live()} non-zero bytes in the context`);
    await this.attach(shimWasm, guestWasm);
    step(`instantiated, ${live()} non-zero bytes in the context`);
    const e = this.#shim.exports;
    // Before the kernel is built, not after: the metadata is what the child's
    // dispatch table starts from, and `gex_adopt_child` reads it as it builds.
    this.#setMetadata(guestWasm);
    const entry = e.gex_adopt_child(
      child.pid,
      child.ctx,
      child.pc,
      child.sp,
      child.tls,
      child.brk ?? 0n,
    );
    step(`kernel built, entry 0x${entry.toString(16)}, ${live()} non-zero bytes in the context`);
    // The kernel exists now, so everything that configures one can be said.
    const o = child.opts ?? {};
    this.#opts = o;
    if (o.strace) e.gex_set_trace(1);
    if (o.tty) e.gex_set_tty(1);
    if (o.uid !== undefined) e.gex_set_identity(o.uid, o.gid ?? o.uid);
    // The map, which the bytes do not carry: without it the child's first
    // `mmap` hands out an address its own image is already at.
    for (const r of child.image) {
      if (r.end !== undefined) e.gex_adopt_vma(BigInt(r.at), BigInt(r.end), r.prot);
    }
    step(`${child.image.length} regions mapped`);
    // Before the modules, because installing one asks the shim which slot
    // each of its blocks belongs in and the answer is in this map.
    if (child.pages?.length && e.gex_adopt_page) {
      const dv = new DataView(child.pages.buffer, child.pages.byteOffset, child.pages.byteLength);
      for (let i = 0; i + 12 <= child.pages.byteLength; i += 12) {
        e.gex_adopt_page(dv.getBigUint64(i, true), dv.getUint32(i + 8, true));
      }
    }
    step('pages adopted');
    // After the kernel exists and before the guest runs: the first thing a
    // forked child does is read or write a descriptor it expects to have.
    if (child.fds?.length && e.gex_adopt_fds) {
      const p = e.gex_alloc(child.fds.length) >>> 0;
      this.#bytes().set(child.fds, p);
      e.gex_adopt_fds(p, child.fds.length);
      e.gex_free?.(p, child.fds.length);
    }
    step(`installing ${child.modules?.length ?? 0} modules`);
    for (const m of child.modules ?? []) await this.#installModule(m.mod, m.need);
    step('running');
    return entry;
  }

  /**
   * Hand the host any child `fork` has just made.
   *
   * Called where the guest gives the stack back, which after a `fork` is the
   * next thing that happens: the parent runs on and this drains the child
   * before it gets far. The environment decides what a Worker *is* -- a page
   * and Node disagree -- so all that happens here is describing the process
   * that needs one.
   */
  /**
   * Hand the host any thread `clone` has just made.
   *
   * A thread is not a process: nothing is copied, because everything it has
   * -- the memory, the kernel inside it, the file table, the thread table --
   * is already there and shared. All that crosses is which entry of the
   * thread table the new Worker is to run.
   *
   * Only ever non-empty when the shim has been told its threads get Workers,
   * which needs a shared memory; otherwise `clone` schedules on this stack
   * as it always did and there is nothing here to drain.
   */
  async #drainThread() {
    const e = this.#shim.exports;
    if (!e.gex_thread_pending) return;
    const tid = e.gex_thread_pending();
    if (!tid) return;
    const thread = { tid, ctx: e.gex_thread_ctx(), pc: e.gex_thread_pc(), memory: this.#memory };
    e.gex_thread_taken();
    if (globalThis.GEX_DEBUG_EXIT) {
      console.error(
        `[gex] thread -> tid ${thread.tid} pc=0x${thread.pc.toString(16)} ` +
          `ctx=0x${thread.ctx.toString(16)}`,
      );
    }
    if (typeof this.#env.spawnThread !== 'function') {
      throw new Error(
        `the guest started thread ${tid} and this environment cannot run one. ` +
          'Give the Env a `spawnThread`, or the thread will never run.',
      );
    }
    await this.#env.spawnThread(thread);
  }

  /**
   * Join a process as one of its threads, and say where to enter.
   *
   * The counterpart of `adoptProcess` and much smaller, which is the point: a
   * process has to be rebuilt from a copy, a thread only has to be pointed at
   * what is already there.
   */
  async adoptThread(shimWasm, guestWasm, thread) {
    await this.attach(shimWasm, guestWasm, { devices: false });
    const entry = this.#shim.exports.gex_adopt_thread(thread.tid);
    if (!entry) throw new Error(`this process has no thread ${thread.tid}`);
    return { entry, ctx: Number(thread.ctx) };
  }

  async #drainFork() {
    const e = this.#shim.exports;
    if (!e.gex_fork_pending) return;
    const pid = e.gex_fork_pending();
    if (!pid) return;
    const child = {
      pid,
      pc: e.gex_fork_pc(),
      ctx: e.gex_fork_ctx(),
      sp: e.gex_fork_sp(),
      tls: e.gex_fork_tls(),
      // Where the parent's heap had reached. The bytes come over in the
      // image; without the number the child's kernel believes it has no heap
      // and grows one from address zero, over its own register contexts.
      brk: e.gex_fork_brk?.() ?? 0n,
      image: this.#processImage(),
      // The map from a guest page to the dispatch slots its blocks are in,
      // and the modules those slots point into. Neither is in the memory --
      // the map is Rust data in this Worker's heap and the modules are
      // compiled wasm the engine holds -- so neither crosses with the bytes.
      //
      // A `WebAssembly.Module` is structured-cloneable, so handing the child
      // these costs nothing: it links them against its own memory and tables
      // without compiling a line.
      pages: this.#pageMap(),
      modules: this.#compiled.slice(),
      opts: this.#opts,
      // The descriptors the child inherits, described rather than cloned: an
      // open file is an `Arc` in the parent's heap and the child's Worker has
      // never seen that heap. Without this a shell pipeline's child cannot
      // read the pipe it was forked to read.
      fds: e.gex_fork_fds_len
        ? this.#bytes().slice(
            e.gex_fork_fds_ptr() >>> 0,
            (e.gex_fork_fds_ptr() >>> 0) + (e.gex_fork_fds_len() >>> 0),
          )
        : null,
    };
    e.gex_fork_taken();
    if (globalThis.GEX_DEBUG_EXIT) {
      // The record as it leaves, not as the child reports it. A child that
      // enters at zero was either handed a zero or lost it on the way, and
      // those are different bugs in different files.
      console.error(
        `[gex] fork -> pid ${child.pid} pc=0x${child.pc.toString(16)} ` +
          `ctx=0x${child.ctx.toString(16)} sp=0x${child.sp.toString(16)} ` +
          `${child.image.length} regions, ${child.modules.length} modules`,
      );
    }
    if (!this.#env.spawnProcess) {
      throw new Error(
        `the guest forked pid ${pid} and this environment cannot start a process. ` +
          'Give the Env a `spawnProcess`, or the child will never run.',
      );
    }
    await this.#env.spawnProcess(child);
  }

  async #catchUp() {
    const e = this.#shim.exports;
    if (!e.gex_module_count) return;
    // A Worker that has attached but not yet been given a process has no
    // kernel to ask, and asking anyway is a panic rather than a zero. That is
    // the order a forked child arrives in: attach, then adopt.
    if (e.gex_has_host && !e.gex_has_host()) return;
    const have = e.gex_module_count() >>> 0;
    // Every slot the *process* has claimed, not the pending module's -- this
    // Worker is catching up on all of them at once.
    const need = (e.gex_jit_slots_total?.() ?? 0) >>> 0;
    for (; this.#installed < have; this.#installed++) {
      const ptr = e.gex_module_ptr(this.#installed) >>> 0;
      const len = e.gex_module_len(this.#installed) >>> 0;
      if (!len) continue;
      // Copied out before compiling: the registry is in the guest's memory,
      // and another thread allocating can move it.
      await this.#installModule(this.#bytes().slice(ptr, ptr + len), need);
    }
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
    this.#bytes().set(bytes, p);
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
    // Before the shim exists as well as after, which is when a forked child
    // is told: a Worker is given its devices between construction and
    // `adoptProcess`, and `load` and `attach` both set this from `#net` on
    // their way through. Reaching for the exports unconditionally made a
    // child with a network fail with `Cannot read properties of undefined`
    // before it ran an instruction.
    this.#shim?.exports?.gex_set_network?.(net ? 1 : 0);
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

  async run(entry, startCtx = 0x1000) {
    // Every switch, signal and miss leaves wasm by trapping, and V8 captures
    // a stack trace for each trap it turns into a RuntimeError. Nothing here
    // ever reads one, and a program with contended locks takes hundreds of
    // thousands of them.
    const limit = Error.stackTraceLimit;
    if (!globalThis.__gexKeepTrace) Error.stackTraceLimit = 0;
    try {
      return await this.#runLoop(entry, startCtx);
    } finally {
      Error.stackTraceLimit = limit;
    }
  }

  async #runLoop(entry, startCtx = 0x1000) {
    let pc = entry;
    // Which thread's registers to enter with. The process's first thread
    // unless told otherwise -- and a thread in a Worker of its own has to say
    // otherwise, or it runs the *main* thread's registers: four threads all
    // entered at 0x1000, re-executed whatever the main thread was in the
    // middle of, and one of them came out having forked.
    let ctx = startCtx;
    for (;;) {
      try {
        // Address zero is never a guest instruction, and entering there is
        // how a lost resume address shows up: `nothing to translate at
        // pc=0x0`, from the dispatch table, several layers away from
        // whatever forgot to say where to go. Named here instead.
        if (!pc) {
          throw new Error(
            `gex: nothing said where to resume (ctx=0x${ctx.toString(16)}). ` +
              'A trap gave the stack back without setting a resume address.',
          );
        }
        this.#guest.exports.enter(ctx, pc);
        // `enter` returning means the guest gave the stack back for good --
        // normally because the last process exited. `GEX_DEBUG_EXIT` says so,
        // because "the run ended" and "the program finished" look identical
        // from outside and are not the same thing.
        if (globalThis.GEX_DEBUG_EXIT) {
          console.error(`[gex] run ended: enter returned, code ${this.#shim.exports.gex_exit_code()}`);
        }
        return this.#shim.exports.gex_exit_code();
      } catch (err) {
        // First, before anything else asks the shim a question: was this the
        // shim handing the stack back, or the shim falling over?
        //
        // Every way out is the same `unreachable` to the engine -- the
        // deliberate handoff, a Rust panic, a failed allocation, a bad access
        // inside the kernel -- and treating them alike reported a runtime
        // that had crashed as a program that exited cleanly with status 0.
        // That is how `apt update` came to print nothing, write no lists and
        // return success. `trap()` sets a flag; nothing else does.
        //
        // It has to be first because the recovery below calls back into the
        // shim, and after a crash the kernel may still be borrowed by
        // whatever crashed -- so those calls fail with a second, misleading
        // error that buries the first.
        if (this.#shim.exports.gex_handoff && !this.#shim.exports.gex_handoff()) {
          // What can still be asked, each on its own, because after a crash
          // the kernel may be borrowed by whatever crashed and a second
          // question would trap on top of the first. Without this the
          // message names the engine's word for every possible cause --
          // "unreachable" -- and nothing else at all.
          const ask = (f, how) => {
            try {
              return f ? how(f()) : '';
            } catch {
              return '';
            }
          };
          const e = this.#shim.exports;
          const at = ask(e.gex_current_pc, (pc) =>
            `, guest pc=0x${(pc < 0n ? pc + (1n << 64n) : pc).toString(16)}`);
          const code = ask(e.gex_exit_code, (c) => (c ? `, exit code ${c}` : ''));
          const heap = ask(e.gex_heap_used_kib, (k) =>
            k >= 0 ? `, ${(k / 1024) | 0} MiB of shim heap in use` : '');
          // The one thing that still answers after the kernel is left
          // borrowed: it is a plain static rather than a question for the
          // kernel, and it narrows a crash to a single handler.
          const nr = ask(e.gex_last_syscall, (n) =>
            n === 0xffffffff ? '' : `, inside syscall ${n}`);
          throw new Error(
            `the gex runtime trapped inside the shim (${err.message}${at}${nr}` +
              `${code}${heap}). That is a crash in the runtime, not a guest exit; ` +
              'anything printed above is all the guest managed to do first.',
          );
        }
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
        this.#shim.exports.__stack_pointer.value = this.#workerStack();
        const e = this.#shim.exports;
        // Drained, not taken one at a time. Statically recompiling a whole
        // object emits as many modules as it needs -- a wasm module holds a
        // million functions and gex emits one per block -- and they have to
        // go in in the order they were emitted, because each one's page
        // assignments are the next one's starting point.
        while (e.gex_jit_pending_len() >>> 0) await this.#installTranslation();
        // And anything another Worker of this process translated.
        //
        // The queue above is one queue for the whole process, because the
        // shim's state is: whichever Worker traps next drains it, installs
        // the module into *its own* dispatch tables, and the Worker that
        // asked for the translation never sees it. Its next branch into that
        // page then finds an empty slot -- `nothing to translate at pc=0x0`,
        // which is what a thread of a threaded program did within a second
        // of starting. `#catchUp` compares this Worker's count against the
        // process's and is a comparison when there is nothing to do.
        await this.#catchUp();
        // A `fork` leaves a child here for the host to start. Drained before
        // the parent runs on, so the two are only ever one process apart --
        // and before the memory is touched again, because what the child gets
        // is a copy of it as `fork` left it.
        await this.#drainFork();
        await this.#drainThread();
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
              // A task rather than a microtask: a microtask would run before
              // Asleep on the shared word, when nothing here is waiting on
              // the page.
              //
              // `gex_park` blocks this Worker inside the global kernel's
              // memory until another Worker bumps the wake counter -- which
              // every syscall does -- or the bound passes. That is a wake
              // rather than a poll: the X server answering its client makes
              // the client runnable in microseconds instead of at whatever
              // interval this loop happened to be turning at.
              //
              // The bound is what keeps the Worker answerable. It is asleep,
              // so the messages the host posts it -- a keystroke, a window
              // size, a child to adopt -- are queued rather than delivered,
              // and the turn below is when that queue runs.
              //
              // -1 is "there is no global kernel to sleep on", and a guest
              // waiting on the network cannot sleep at all: the bytes it
              // wants arrive on this Worker's own event loop.
              if (e.gex_paused_for_host() || e.gex_park(20) < 0) {
                // See `#nap` for why this is not a bare `setTimeout`.
                await this.#nap();
              } else {
                await this.#turn();
              }
            }
            woke = e.gex_wake();
            if (woke === 1) break;
          }
          if (woke !== 1) {
            // Every thread is waiting and nothing woke them. That is the end
            // of the run whether or not the guest is finished, so it is worth
            // being able to tell the two apart.
            if (globalThis.GEX_DEBUG_EXIT) {
              console.error(`[gex] run ended: nothing woke (wake=${woke}), code ${e.gex_exit_code()}`);
            }
            return e.gex_exit_code();
          }
          pc = e.gex_resume_pc();
          const wakeCtx = e.gex_resume_ctx() >>> 0;
          if (wakeCtx !== 0) ctx = wakeCtx;
          continue;
        }
        // Anything that gets this far is a trap nothing here expects, and
        // the message alone -- "memory access out of bounds" -- says what
        // went wrong and nothing about where. Translated code faulting that
        // way means the guest computed a wild pointer, so the guest's own
        // program counter is the question, and which library it is in.
        if (e.gex_current_pc && !String(err).includes('unreachable')) {
          const pc = e.gex_current_pc();
          let where = '';
          if (e.gex_object_at) {
            const at = e.gex_alloc(160) >>> 0;
            const n = e.gex_object_at(pc, at, 160) >>> 0;
            if (n) where = ' in ' + new TextDecoder().decode(this.#bytes().subarray(at, at + n));
          }
          const ctx = e.gex_current_ctx ? e.gex_current_ctx() : 0n;
          // And how full the shim's own heap was. That heap is a fixed region
          // -- the memory belongs to the guest and cannot grow -- so running
          // out of it is a real failure mode whose only visible symptom is a
          // fault somewhere in the guest, at whatever address the guest was
          // last recorded at, nowhere near the allocation that failed. A
          // number here is the difference between reading that message as a
          // wild pointer and reading it as a heap that ran out. Present only
          // in a build with `--features heap-counters`.
          const used = e.gex_heap_used_kib?.() ?? -1;
          const total = e.gex_heap_total_kib?.() ?? -1;
          const heap =
            used >= 0 && total > 0
              ? `, heap ${(used / 1024).toFixed(0)}/${(total / 1024).toFixed(0)} MiB`
              : '';
          // And where dispatch thought that address lived. "table index is
          // out of bounds" means a `return_call_indirect` ran off the end of
          // whichever table the page directory named, and the two numbers
          // that settle it are the slot it wanted and how long that table
          // actually is -- neither of which the engine's message carries.
          let dispatch = '';
          if (e.gex_dispatch_slot) {
            const packed = e.gex_dispatch_slot(pc);
            const tables = this.#dispatchTables();
            const lens = tables.map((t) => t.length).join(',');
            dispatch =
              packed < 0
                ? `, no slots for this page, tables ${lens}`
                : `, table ${packed >>> 24} slot ${packed & 0xffffff}, tables ${lens}`;
          }
          err.message +=
            ` (guest pc=0x${(pc < 0n ? pc + (1n << 64n) : pc).toString(16)}${where}` +
            `, ctx=0x${ctx.toString(16)}${heap}${dispatch})`;
        }
        if (String(err).includes("unreachable")) {
          // The ordinary end: the shim set an exit status and abandoned the
          // wasm stack. `enter` never returns in that case, so this is the
          // path a finished program actually takes.
          if (globalThis.GEX_DEBUG_EXIT) {
            // With where it was, because this path is taken by a program that
            // finished *and* by one that crashed with no status set -- and
            // those look identical otherwise. apt's forked helper came
            // through here as "code 0" while apt sat reading the pipe it
            // never wrote to.
            const say = (fn, f) => {
              try {
                return fn ? f(fn()) : '';
              } catch {
                return '';
              }
            };
            const where =
              say(e.gex_current_pc, (pc) => `, pc=0x${(pc < 0n ? pc + (1n << 64n) : pc).toString(16)}`) +
              say(e.gex_last_syscall, (n) => (n === 0xffffffff ? '' : `, in syscall ${n}`));
            // And how much of this process's memory the shim actually used,
            // which is the number that decides how big a memory has to be --
            // and therefore how many processes a browser can hold at once.
            const heap = say(e.gex_heap_used_kib, (k) =>
              k >= 0 ? `, shim heap ${(k / 1024).toFixed(1)} MiB` : '');
            console.error(
              `[gex] run ended: shim trapped out, code ${e.gex_exit_code()}${where}${heap}`,
            );
          }
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
    await this.#installModule(this.#bytes().slice(ptr, ptr + len), e.gex_jit_slots() >>> 0);
    e.gex_jit_installed();
    // This Worker translated it, so it is already the newest one installed
    // here; the registry entry `gex_jit_installed` just added is the same
    // module and must not be replayed on top of itself.
    this.#installed = e.gex_module_count ? e.gex_module_count() >>> 0 : this.#installed;
  }

  /**
   * Put one translated module's blocks into this Worker's dispatch tables.
   *
   * Shared by the two ways a module arrives: this Worker translated it, or
   * another thread did and this one is catching up. The work is the same
   * either way, which is the point of it being one function -- a thread that
   * installed modules differently from the thread that made them would
   * dispatch to different code.
   */
  async #installModule(bytes, need) {
    const e = this.#shim.exports;
    const tables = this.#dispatchTables();
    // The shim allocates in one slot space that spans every table, so the
    // figure it reports is a running total and each table's share is that
    // total clamped to its own window. Several tables exist because V8 caps
    // one at ten million entries, which is 38 MiB of guest text -- less than
    // half the graphics rootfs. See #64.
    //
    // Finite, not `Infinity`: the window arithmetic multiplies this by the
    // table index, and `0 * Infinity` is `NaN`, which reaches `Table.grow`
    // as "Argument 0 must be convertible to a valid number" and says nothing
    // about where it came from. A shim from before this export existed has
    // one table, so any number too large to reach is the right answer.
    const per = (e.gex_slots_per_table?.() ?? 0) >>> 0 || Number.MAX_SAFE_INTEGER;
    // Table 0's slot 0 is the miss stub, and it is what an unfilled slot in
    // any table must hold: a branch to an address nothing has translated has
    // to reach the host, not a stale function. Tables past the first start
    // empty, so they have no element 0 of their own to grow from.
    const stub = tables[0].get(0);
    for (let i = 0; i < tables.length; i++) {
      const want = Math.min(Math.max(need - i * per, 0), per);
      if (tables[i].length >= want) continue;
      try {
        tables[i].grow(want - tables[i].length, stub);
      } catch (err) {
        throw new Error(
          `dispatch table ${i} would not grow from ${tables[i].length} to ${want}: ${err.message}`,
        );
      }
    }
    const mod = bytes instanceof WebAssembly.Module ? bytes : await WebAssembly.compile(bytes);
    this.#compiled.push({ mod, need });
    const imports = { ...this.#imports };
    for (let i = 0; i < tables.length; i++) imports[TABLE_NAMES[i]] = tables[i];
    const inst = await WebAssembly.instantiate(mod, { gex: imports });
    for (const name of Object.keys(inst.exports)) {
      if (name[0] !== 'b') continue;
      // The shim packs the table index into the top eight bits, the same way
      // a directory entry does.
      const packed = e.gex_dispatch_slot(BigInt('0x' + name.slice(1)));
      if (packed < 0) continue;
      tables[packed >>> 24].set(packed & 0xffffff, inst.exports[name]);
    }
  }

  /** Every dispatch table the guest module exports, in index order. */
  #dispatchTables() {
    const out = [];
    for (const n of TABLE_NAMES) {
      const t = this.#guest.exports[n];
      if (!t) break;
      out.push(t);
    }
    return out;
  }

}
