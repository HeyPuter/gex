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
// Where the directory itself starts: one `u32` per 4 KiB guest page, holding
// the packed table index and slot a branch into that page dispatches to.
// How many traps a forked child takes before it gives up on being an
// `execve` and takes its parent's modules in bulk. Zero or one is the old
// eager behaviour, deferred by a trap.
// Read where it is used, not here.
//
// As a module-level constant this was evaluated when the module was first
// imported, which is before the page has had a chance to set the global --
// so `?adopt=0` selected nothing and every run that was supposed to be
// comparing eager against deferred was comparing deferred against itself.
// Three bisect conclusions were drawn from that before the bug was noticed.
function adoptAfterTraps() {
  // One -- deferred. A forked child takes its parent's modules without
  // installing them, and picks up what it actually branches into.
  //
  // This was zero for a long time, and the note here said eager was "the only
  // setting that is correct": rebuilding the inherited directory had been
  // tried three ways and each ended with a grandchild crashing in an
  // unrelated syscall, or a child that adopted and then never ran. The
  // conclusion drawn was that something a child needs travels in that
  // directory.
  //
  // It does not. Those were `enter` failing to mask the table index out of a
  // directory entry -- a rebuilt directory hands out entries in table 1, and
  // `enter` indexed table 0 with the table number still in the top eight
  // bits, which is a branch 16.8 million slots past the end. Every one of the
  // three attempts was correct and every one of them tripped the same bug.
  // With that fixed (3ecbb8a, and actually shipped in d0795fd) deferring
  // works, and a forked child costs
  //
  //     66ms to adopt, against 356ms
  //
  // because it neither grows its dispatch tables to hold 1,121 inherited
  // modules -- 269ms of allocating and stub-filling before it runs an
  // instruction -- nor installs them, when almost every child `execve`s
  // something else immediately and throws the lot away.
  return Number(globalThis.GEX_ADOPT_AFTER ?? 1);
}

// What a module calls the function that writes its passive element segments
// into the tables it imports. See `Module::add_fill`.
const FILL = 'gexfill';
// And the one a miss can ask for: fill whichever segment holds this slot.
const FILL_AT = 'gexfillat';
const FILL_ALL = 'gexfillall';

// Above this, a module's slots are written when a branch asks for them
// rather than when it is instantiated.
//
// Small units are wanted immediately and cost almost nothing to write; the
// closure's libraries are the expensive ones and are mostly never reached.
// `libgcrypt` is 303 MiB of wasm and apt runs a sliver of it. Four megabytes
// is provisional and wants a measurement of what a fill costs per byte --
// the last number picked without one was sixty-four, against libc's 355
// segments.
const FILL_ON_DEMAND_BYTES = 4 << 20;

const DISPATCH_DIR = 0x00100000;
const LOW_END = DISPATCH_DIR + 0x80000 * 4;
const PAGES = GUEST_BYTES / 65536;
const INITIAL_PAGES = GUEST_INITIAL_BYTES / 65536;

/**
 * A cheap content hash, for saying whether two copies of a region agree.
 *
 * FNV-1a over words rather than bytes: this runs over a whole process image
 * and only ever answers same-or-different, so a quarter of the reads is worth
 * more than the collision resistance it gives up.
 */
function sum32(buf) {
  const u = new Uint32Array(buf.buffer ?? buf, buf.byteOffset ?? 0, (buf.byteLength / 4) | 0);
  let h = 0x811c9dc5;
  for (let i = 0; i < u.length; i++) h = Math.imul(h ^ u[i], 0x01000193);
  return h >>> 0;
}

/** Everything the host has to supply that wasm cannot do for itself. */

/**
 * How far into the slot space a module's element segments reach, read from
 * its own `gex.meta`.
 *
 * The number a caller passes as `need` cannot be trusted, because for a
 * closure library it is deliberately zero: registration reserves nothing and
 * `#publish` grows the tables when a branch first wants one of its pages.
 * That is fine for the library itself and wrong for everyone who inherits
 * it. A forked child adopts its parent's modules with the parent's `need`,
 * so a closure library reaches `#adoptModule` claiming zero slots, nothing
 * grows, and instantiating it writes past the end of the table:
 *
 *   installing a module of 0 slots failed: table index is out of bounds.
 *   tables are [1133568, 0, 0, 0, 283648, 0, 0, 0]
 *
 * 1,133,568 is exactly `libapt-pkg`'s `next_slot`, so the tables had grown
 * for the libraries that were published and stopped there. apt-key forks,
 * the child adopts, and the run ends.
 *
 * The metadata is beside the module in every one of these paths and says the
 * truth: magic, text base, table slots, then `next_slot` at offset 24.
 */
function slotsNeeded(meta) {
  if (!meta || meta.length < 32) return 0;
  const v = new DataView(meta.buffer, meta.byteOffset, meta.byteLength);
  return Number(v.getBigUint64(24, true));
}


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
export function metadataSection(wasm) {
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
      // A copy, not a view.
      //
      // `subarray` shares the module's `ArrayBuffer`, and `postMessage`
      // clones a typed array by cloning the buffer *behind* it -- so sending
      // a closure's metadata to a Worker sent all 49 compiled modules with
      // it, 231 MB, once per process. It measured as 432ms and 856ms in a
      // line that claims to send "49 closure modules" and was supposed to be
      // sending fifty kilobytes of page tables.
      if (name === 'gex.meta') return wasm.slice(i + nameLen, end);
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
  // What compiling and instantiating shared modules costs this process, and
  // how many there were. See `#installModuleInner`.
  #installMs = 0;
  /// `WebAssembly.compile` and `WebAssembly.instantiate`, separately.
  ///
  /// "Installing" has been one number, and it is two very different costs
  /// with two entirely different fixes. Compiling is per *bytes* and V8 will
  /// skip it for a module it has seen before -- if the bytes are identical.
  /// Instantiating is per *module* and is paid every time whatever the bytes
  /// are. Which of them dominates decides whether the answer is a code cache
  /// or fewer, larger modules, and no counter here could tell them apart.
  #compileMs = 0;
  #instantiateMs = 0;
  #growMs = 0;
  /// What every Worker pays before the guest runs an instruction.
  ///
  /// The shim is nine megabytes and the guest module is its own, and both are
  /// instantiated per Worker -- one Worker being one guest process. None of
  /// it was in any counter: `installMs` covers translated modules only, so
  /// the floor under every process has been assumed small rather than
  /// measured.
  #shimCompileMs = 0;
  #shimInstantiateMs = 0;
  #guestInstantiateMs = 0;
  #installs = 0;
  // Instances whose slots have not been written, newest first: a miss asks
  // each in turn whether it holds the slot. See `fillSlot`.
  #unfilled = [];
  /** Closure libraries that arrived after this process started. */
  #streamed = 0;
  /** Closure libraries registered but not yet instantiated, in shim order. */
  #lazyClosure = [];
  /** Inherited modules registered but not yet instantiated, in shim order. */
  #lazyInherited = [];
  // Fill requests answered, and requests nobody could answer.
  #filled = 0;
  #unanswered = 0;
  // The parent's modules, held rather than installed. See `#maybeAdopt`.
  #inherited = [];
  #traps = 0;
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
      memory ?? new WebAssembly.Memory({ initial: INITIAL_PAGES, maximum: PAGES, shared });
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
    // A 64-bit memory counts its pages in BigInt, here as everywhere else.
    const pages = Math.ceil((end - have) / 65536);
    this.#memory.grow(pages);
    this.#u8 = null;
  }

  /** Closure library modules waiting for the guest instance to exist. */
  #closureModules = [];
  /// Ask whoever holds the closure for library `i`, compiled. Null when the
  /// caller handed the modules over up front, which the page still does for
  /// pid 1 -- it compiled them itself and has nowhere to ask.
  #wantLibrary = null;

  #bytes() {
    if (!this.#u8 || this.#u8.byteLength === 0) {
      this.#u8 = new Uint8Array(this.#memory.buffer);
    }
    return this.#u8;
  }

  get memory() { return this.#memory; }

  /** The shim instance, for diagnostics. */
  /**
   * Let go of everything that would keep a host's event loop alive.
   *
   * The yielding channel is the one that matters. `#tick` is a
   * `MessageChannel` with `port1.start()` called on it, and in node a started
   * port is an *active handle*: it keeps the process alive on its own, for as
   * long as it is open. A page does not care, which is why this went
   * unnoticed -- a tab is alive anyway.
   *
   * Under node it meant a run that ever yielded never exited. `echo hi`
   * finished and left, because a single process that never waits never makes
   * the channel; `ls /bin | head -3` made one the moment the shell waited for
   * a child, and then node sat there with the pipeline's output already
   * written and every process reaped. The run looked like a hang and was
   * really a finished program the host would not let go of.
   *
   * Not `unref()`, which node's ports do have: unref'd, the channel stops
   * being a reason to stay alive *while the run is still using it*, and a
   * parent between ticks with no child running would have nothing ref'd at
   * all. Closing it after the run is over says the right thing.
   */
  close() {
    if (this.#tick) {
      this.#tick.port1.close();
      this.#tick.port2.close();
      this.#tick = null;
    }
  }

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
  /** Milliseconds this process spent compiling and instantiating modules. */
  get installMillis() { return Math.round(this.#installMs); }
  get compileMillis() { return Math.round(this.#compileMs); }
  get instantiateMillis() { return Math.round(this.#instantiateMs); }
  get growMillis() { return Math.round(this.#growMs); }
  /// How many syscalls the guest's dynamic linker made, and when it made its
  /// last one. See `gex_loader_calls`: a closure knows the layout at build
  /// time and the run rediscovers it.
  get loader() {
    const e = this.#shim?.exports;
    if (!e?.gex_loader_calls) return null;
    return {
      calls: Number(e.gex_loader_calls()),
      doneMs: Number(e.gex_loader_done_ms()),
      // What the linked-image store did for this process, if anything. On
      // this line because it is the one that always reaches the console: the
      // shim's own narration goes to the guest's stderr, and the arming
      // happens before a test's terminal has twenty lines of scrollback.
      link: ['off', 'no-kernel', 'armed', 'kept', 'restored', 'asked'][
        Number(e.gex_link_state?.() ?? 0)
      ],
      // What putting the address space back cost, when one was put back:
      // regions, bytes re-read from files, bytes copied out of the image,
      // and the milliseconds in each. The restored process turned out to
      // spend about what the linker it skipped had spent, and nothing said
      // where.
      cost: e.gex_link_cost ? (() => {
        const p = e.gex_alloc(40) >>> 0;
        e.gex_link_cost(p);
        const v = new BigUint64Array(this.#bytes().buffer, p, 5);
        return [Number(v[0]), Number(v[1]), Number(v[2]), Number(v[3]), Number(v[4])];
      })() : null,
    };
  }
  /// Which objects this process lifted blocks from, `name=count` separated.
  get blocksByObject() {
    const e = this.#shim?.exports;
    if (!e?.gex_jit_by_object) return '';
    const max = 512;
    const p = e.gex_alloc(max) >>> 0;
    const n = e.gex_jit_by_object(p, max) >>> 0;
    const s = new TextDecoder().decode(this.#bytes().subarray(p, p + n));
    e.gex_free?.(p, max);
    return s;
  }
  get shimCompileMillis() { return Math.round(this.#shimCompileMs); }
  get shimInstantiateMillis() { return Math.round(this.#shimInstantiateMs); }
  get guestInstantiateMillis() { return Math.round(this.#guestInstantiateMs); }
  /** How many modules that was. */
  get installCount() { return this.#installs; }

  /** Fill requests answered, and requests nobody could answer. */
  get fillStats() { return { filled: this.#filled, unanswered: this.#unanswered }; }

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
    // A `WebAssembly.Module` if the host has one, bytes if it has not.
    //
    // The shim is 8.7 MB, and this used to compile it in every Worker: ten
    // guest processes meant ten compilations of the same module, which is
    // most of what `apt-get update` spent its CPU on -- four background
    // threads at 28 seconds each, matched to a tenth of a second, which is a
    // thread pool and not a guest.
    //
    // A compiled module is structured-cloneable and the engine shares the
    // code behind it, so passing one costs a message and saves the compile.
    const shimMod =
      shimWasm instanceof WebAssembly.Module ? shimWasm : await WebAssembly.compile(shimWasm);
    await this.#startKernel();
    this.#shim = await WebAssembly.instantiate(shimMod, {
      env: { memory: this.#memory },
      // The shim asks JS for exactly the two things wasm has no way to do.
      gex_env: this.#hostImports(),
      gex_k: this.#kernelImports(shimMod),
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
    // How many of an object's stored units one miss may take. Swept rather
    // than assumed: the 16 it defaults to was chosen against 4096, which only
    // ruled out unbounded.
    if (opts.perMiss) this.#shim.exports.gex_set_per_miss?.(opts.perMiss);
    // Keep what the guest's dynamic linker produces, and start the next
    // process of the same program from it instead of linking again.
    if (opts.link) this.#shim.exports.gex_set_snapshot?.(Number(opts.link));
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
    // Freed, one file at a time.
    //
    // `gex_add_file` copies what it is given -- `shim_slice(..).to_vec()` --
    // so the buffer handed over is dead the moment it returns, and nothing
    // used to say so. Every file's bytes stayed allocated in this process's
    // linear memory for the life of the process, on top of the copy the
    // filesystem kept, so a rootfs cost twice its size and half of that was
    // never read again.
    //
    // It is worth what it sounds like. A guest mounting Debian's 226 MB
    // rootfs reached 560 MiB before executing an instruction; the same guest
    // with a 1.9 MB rootfs reached 96 MiB. With the free, the allocator hands
    // the same span back for the next file, so the peak is the largest single
    // file rather than the sum of all of them -- and a browser holds one
    // memory per process, which is what makes this the number that decides
    // how many processes a tab can run.
    const enc = new TextEncoder();
    const free = this.#shim.exports.gex_free;
    // Empty directories first, so a file arriving under one finds it.
    //
    // `gex_add_file` makes the directories on the way to each file, so a
    // tree with content in it arrives intact and an empty directory arrives
    // not at all. `web/src/tar.js` collects them into `dirs` precisely so
    // this can exist, and until now nothing passed them: apt met
    // "Archives directory /var/cache/apt/archives/partial is missing" for a
    // directory that is in the tar.
    for (const path of opts.dirs ?? []) {
      const p = enc.encode(path);
      const pp = this.#shim.exports.gex_alloc(p.length) >>> 0;
      this.#bytes().set(p, pp);
      this.#shim.exports.gex_add_dir?.(pp, p.length);
      free?.(pp, p.length);
    }
    for (const [path, contents] of Object.entries(files)) {
      const p = enc.encode(path);
      const d = contents instanceof Uint8Array ? contents : enc.encode(contents);
      const pp = this.#shim.exports.gex_alloc(p.length) >>> 0;
      const dp = this.#shim.exports.gex_alloc(d.length) >>> 0;
      this.#bytes().set(p, pp);
      this.#bytes().set(d, dp);
      this.#shim.exports.gex_add_file(pp, p.length, dp, d.length);
      // Optional so that an older shim still loads; it simply keeps the old
      // behaviour of never letting go.
      free?.(pp, p.length);
      free?.(dp, d.length);
    }

    // The layout a closure build assumed, before the program is loaded and
    // long before `ld.so` reserves anything. Every module in a statically
    // recompiled closure has its object's guest addresses baked in, so this
    // is a precondition rather than a hint -- and the guest cooperates
    // without knowing it, because `mmap(NULL, span)` takes what it is given.
    // Which program the image module was built from, so the shim can refuse
    // to hand its pages to a process running something else. See
    // `image_pages_are_ours`.
    if (opts.program && this.#shim.exports.gex_set_image_program) {
      const b = enc.encode(opts.program);
      const bp = this.#shim.exports.gex_alloc(b.length) >>> 0;
      this.#bytes().set(b, bp);
      this.#shim.exports.gex_set_image_program(bp, b.length);
      free?.(bp, b.length);
    }
    const pin = this.#shim.exports.gex_pin_object;
    for (const [path, where] of Object.entries(opts.pins ?? {})) {
      if (!pin) break;
      const b = enc.encode(path);
      const bp = this.#shim.exports.gex_alloc(b.length) >>> 0;
      this.#bytes().set(b, bp);
      // The span as well as the base: the placement table needs to know how
      // much of the address space a pinned object owns, or it hands the next
      // one an address inside it.
      pin(bp, b.length, BigInt(where.base ?? where), BigInt(where.span ?? 0));
      free?.(bp, b.length);
    }

    const ptr = this.#shim.exports.gex_alloc(elf.length) >>> 0;
    this.#bytes().set(elf, ptr);
    const argvBlob = new TextEncoder().encode(args.join('\0'));
    let ap = 0;
    if (argvBlob.length) {
      ap = this.#shim.exports.gex_alloc(argvBlob.length) >>> 0;
      this.#bytes().set(argvBlob, ap);
    }
    // Phase markers, because a boot that never finishes says nothing about
    // where it stopped. A 694 MB closure under a shell reaches none of these.
    if (globalThis.GEX_DEBUG_EXIT) {
      console.error(`[gex] load: ${Object.keys(opts.pins ?? {}).length} pins, `
        + `${Object.keys(files).length} files staged, loading the program`);
    }
    const entry = this.#shim.exports.gex_load(ptr, elf.length, ap, argvBlob.length);
    if (entry === 0n) throw new Error('gex: could not load the program');
    if (globalThis.GEX_DEBUG_EXIT) {
      console.error(`[gex] load: program at ${entry}, `
        + `${(opts.closure ?? []).length} closure modules to install`);
    }

    // The rest of the closure: one module per shared library, translated
    // ahead of time by the same `gexc` run that produced the image module
    // above. Held back until now because the image has to be placed first --
    // its metadata is what says how far the dispatch tables already reach --
    // and installed before the guest runs, so `ld.so` never takes a dispatch
    // miss in a library at all.
    //
    // Deferred one step: the tables the modules import belong to the guest
    // instance, which is built below. `#closure` is what installs them.
    this.#closureModules = opts.closure ?? [];
    this.#wantLibrary = opts.wantLibrary ?? this.#wantLibrary;


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
    const tGuest = performance.now();
    this.#guest = await WebAssembly.instantiate(
      guestWasm instanceof WebAssembly.Module ? guestWasm : await WebAssembly.compile(guestWasm),
      { gex: this.#imports },
    );
    this.#guestInstantiateMs = performance.now() - tGuest;
    this.#offerToFill(this.#guest);
    if (globalThis.GEX_DEBUG_EXIT) console.error('[gex] load: instantiated, installing');
    await this.#installClosure();
    if (globalThis.GEX_DEBUG_EXIT) console.error('[gex] load: done');
    return entry;
  }

  /**
   * Take one module a fork handed over, without instantiating it yet.
   *
   * A child inherits its parent's whole history and branches into almost
   * none of it: about 485 modules in an `apt-get update`, and 19 seconds of
   * `WebAssembly.instantiate` across twenty-six processes. The directory it
   * inherited already names these pages, so the entries have to be published
   * now -- but the instance can wait for a branch, exactly as a closure
   * library does.
   *
   * Without metadata there is nothing to publish and nothing to key a later
   * fill on, so those install the old way. A module built before this existed
   * has none, and so does the image.
   */
  async #adoptModule(m) {
    const e = this.#shim.exports;
    // What this module actually reaches, whatever the caller said. See
    // `slotsNeeded`: a closure library arrives here claiming zero.
    const truly = Math.max(m.need >>> 0, slotsNeeded(m.meta));
    if (!m.meta || !m.meta.length || !e.gex_add_lazy_module) {
      await this.#installModule(m.mod, truly);
      return;
    }
    const gp = e.gex_alloc(m.meta.length) >>> 0;
    this.#bytes().set(m.meta, gp);
    const need = e.gex_add_lazy_module(gp, m.meta.length) >>> 0;
    e.gex_free?.(gp, m.meta.length);
    if (!need) {
      await this.#installModule(m.mod, truly);
      return;
    }
    this.#growTables(Math.max(need, truly));
    this.#lazyInherited.push({ mod: m.mod, need: truly, inst: null, meta: m.meta });
  }

  /**
   * Install the closure's library modules, now that the tables exist.
   *
   * Each was built by the same `gexc` run as the image module, against one
   * page table, so their slots cannot collide with each other or with the
   * image. The shim reads each one's `gex.meta` for the pages it covers and
   * the reach it needs, writes the guest's dispatch directory, and answers
   * with the figure the tables must be grown to.
   *
   * A module whose metadata will not read is skipped rather than installed:
   * its blocks would go into slots nothing had claimed, and the failure would
   * surface as a branch into the wrong function much later.
   */
  /**
   * Install one closure library, arriving after the process is already up.
   *
   * The whole closure cannot be handed to a Worker at once: eighteen compiled
   * modules is 694 MB of `WebAssembly.Module` and `postMessage` answers
   * `DataCloneError: Data cannot be cloned, out of memory`. They come one at a
   * time instead, and the guest runs while they arrive -- which is safe
   * because a page whose module has not landed yet is exactly what the miss
   * path forgives: it asks once, nobody can fill it, and it asks again when
   * the next deferred module shows up.
   */
  async installClosureModule(entry) {
    if (!this.#shim || !entry) return false;
    const meta = entry instanceof Uint8Array ? metadataSection(entry) : entry.meta;
    if (!meta || !meta.length) return false;
    const add = this.#shim.exports.gex_add_closure_module;
    if (!add) return false;
    const gp = this.#shim.exports.gex_alloc(meta.length) >>> 0;
    this.#bytes().set(meta, gp);
    const need = add(gp, meta.length) >>> 0;
    this.#shim.exports.gex_free?.(gp, meta.length);
    if (!need) return false;
    const mod = entry instanceof Uint8Array ? entry : entry.mod;
    // Registered and reserved, not instantiated.
    //
    // The directory now points at this library's pages and the tables are
    // long enough to hold its slots, so a branch into it resolves and faults
    // where it should. What has not happened is `WebAssembly.instantiate`,
    // and that is the point: apt's closure is twenty-nine libraries and a
    // process instantiated all of them at boot -- 37 seconds of a 61 second
    // run -- to reach a handful. `libgcrypt` alone is 304 MiB.
    //
    // `fillSlot` builds the instance when a branch actually wants one of
    // these pages. Everything downstream already tolerates the wait: a page
    // nobody can fill yet is forgiven and asked again.
    this.#growTables(need);
    this.#lazyClosure.push({ mod, need, inst: null });
    this.#streamed++;
    return true;
  }

  async #installClosure() {
    const add = this.#shim.exports.gex_add_closure_module;
    if (!add) return;
    // The highest slot total any of them needs, so the tables grow once.
    let most = 0;
    for (const [i, entry] of this.#closureModules.entries()) {
      // Either the raw wasm, or the pair the page prepared: the metadata
      // section on its own and the module already compiled.
      //
      // The pair is what a page should send. A closure is hundreds of
      // megabytes -- apt's is 680 -- and the bytes exist only to be compiled
      // and to have one custom section read out of them. Sending them to
      // every Worker meant structured-cloning that per process and compiling
      // it per process, for ten processes, when `WebAssembly.Module` crosses
      // a `postMessage` as itself and the engine shares the code behind it.
      const paired = !(entry instanceof Uint8Array);
      const wasm = paired ? null : entry;
      const meta = paired ? entry.meta : metadataSection(entry);
      let need = 0;
      if (meta && meta.length) {
        // Timed per library. Registering a closure costs a forked child
        // 399ms -- more than the translation the closure saves it -- and
        // eighteen libraries across the eighty-five processes of an
        // `apt-get update` is thirty-four seconds. Which of the three steps
        // it is (allocating and copying the metadata into guest memory,
        // decoding it, or writing a directory entry per covered page)
        // decides the fix, and none of them had been measured.
        const t0 = performance.now();
        const gp = this.#shim.exports.gex_alloc(meta.length) >>> 0;
        this.#bytes().set(meta, gp);
        const tCopied = performance.now();
        // With the program this library is the image of, when it is one.
        // Every root of a closure is an image and they all claim `PIE_BASE`,
        // so the runtime tells them apart by program rather than by page.
        const prog = new TextEncoder().encode(paired ? (entry.program ?? '') : '');
        let pp = 0;
        if (prog.length) {
          pp = this.#shim.exports.gex_alloc(prog.length) >>> 0;
          this.#bytes().set(prog, pp);
        }
        need = add(gp, meta.length, pp, prog.length) >>> 0;
        if (pp) this.#shim.exports.gex_free?.(pp, prog.length);
        const tAdded = performance.now();
        this.#shim.exports.gex_free?.(gp, meta.length);
        if (globalThis.GEX_DEBUG_EXIT) {
          console.error(
            `[closure] library ${i}: ${(meta.length / 1024) | 0} KiB of metadata, `
            + `${Math.round(tCopied - t0)}ms copying, `
            + `${Math.round(tAdded - tCopied)}ms registering, ${need} slots`,
          );
        }
      }
      // `need` is 0 now: registration reserves nothing and publishes
      // nothing, so there is nothing to grow for yet. The metadata is kept
      // because `fillSlot` publishes from it.
      void need;
      // Registered and reserved, not instantiated.
      //
      // The directory now points at this library's pages and the tables are
      // long enough for its slots, so a branch into it resolves and faults
      // where it should. What has not happened is the instantiate, and that
      // is the point: apt's closure is twenty-nine libraries, a process built
      // every one of them at boot -- 37 of a 61 second run -- and apt reaches
      // a handful. `libgcrypt` alone is 304 MiB.
      //
      // `fillSlot` builds one when a branch wants a page it covers. The wait
      // is already tolerated: a page nobody can fill yet is forgiven and
      // asked again when something new arrives.
      // Grown once, after the loop. `Table.grow` reallocates, and `need` is
      // a running total that reaches 1,607,680 across eighteen libraries --
      // so growing per library copies an ever-larger table eighteen times.
      // That is the 399ms every forked child was spending here, against 1ms
      // of actually registering them, and it is what made the closure cost a
      // short-lived process more than it saved: `/bin/true` lived 1966ms with
      // the closure and 1378ms without.
      most = Math.max(most, need);
      this.#lazyClosure.push({
        mod: paired ? entry.mod : wasm,
        // From the metadata, not from `need`, which registration returns as
        // zero on purpose. `#publish` grows the tables when a page of this
        // library is first wanted, and every other route to instantiating it
        // has to grow for itself.
        need: Math.max(need, slotsNeeded(meta)),
        inst: null,
        meta,
        published: false,
      });
    }
    // Nothing is grown here any more.
    //
    // It used to grow to the highest slot any registered library needed --
    // the whole closure -- and that was the single largest per-process cost
    // left: 405ms in pid 1 and 524ms in a forked child, for 2,850,816 slots
    // and 22 MB of table, in every one of the hundred processes an
    // `apt-get update` starts. `#publish` grows for a library when a branch
    // asks for one of its pages.
    void most;
    this.#closureModules = [];
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
    // A `WebAssembly.Module` if the host has one, bytes if it has not.
    //
    // The shim is 8.7 MB, and this used to compile it in every Worker: ten
    // guest processes meant ten compilations of the same module, which is
    // most of what `apt-get update` spent its CPU on -- four background
    // threads at 28 seconds each, matched to a tenth of a second, which is a
    // thread pool and not a guest.
    //
    // A compiled module is structured-cloneable and the engine shares the
    // code behind it, so passing one costs a message and saves the compile.
    const tShimC = performance.now();
    const shimMod =
      shimWasm instanceof WebAssembly.Module ? shimWasm : await WebAssembly.compile(shimWasm);
    this.#shimCompileMs = performance.now() - tShimC;
    const tShimI = performance.now();
    this.#shim = await WebAssembly.instantiate(shimMod, {
      env: { memory: this.#memory },
      gex_env: this.#hostImports(),
      gex_k: this.#kernelImports(shimMod),
    });
    this.#shimInstantiateMs = performance.now() - tShimI;
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
    const tGuest = performance.now();
    this.#guest = await WebAssembly.instantiate(
      guestWasm instanceof WebAssembly.Module ? guestWasm : await WebAssembly.compile(guestWasm),
      { gex: this.#imports },
    );
    this.#guestInstantiateMs = performance.now() - tGuest;
    this.#offerToFill(this.#guest);
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
    const t0 = performance.now();
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
    // What each region looked like as it left, so the child can say whether
    // what it is running on is what was sent.
    //
    // `apt update` corrupts a forked child's heap -- glibc catches it itself,
    // `malloc_consolidate(): invalid chunk size`, and aborts. A copy that
    // arrives wrong and a copy that arrives right under a kernel that
    // disagrees about where the heap ends are completely different bugs and
    // look identical from outside, which is where #96 sat for three days.
    if (globalThis.GEX_DEBUG_EXIT) for (const r of out) r.sum = sum32(r.bytes);
    // What a fork costs the parent, which is not free and has never been
    // timed: every mapped region is copied out of the memory, and the copy
    // then crosses to the child's Worker. `apt-get update` forks eighty-five
    // times.
    if (globalThis.GEX_DEBUG_EXIT) {
      const bytes = out.reduce((n, r) => n + r.bytes.byteLength, 0);
      // And how much of it the child could have re-read instead. See
      // `gex_vma_reloadable`: a read-only mapping with an origin is library
      // text, identical to the file it came from, and copying it to the child
      // is copying something the child could open.
      let again = 0;
      if (e.gex_vma_reloadable) {
        for (let i = 0; i < n; i++) {
          if (e.gex_vma_at(i, scratch) !== 0) continue;
          if (!e.gex_vma_reloadable(i)) continue;
          const a = Number(dv.getBigUint64(scratch, true));
          const b = Number(dv.getBigUint64(scratch + 8, true));
          if (b > a) again += b - a;
        }
      }
      console.error(
        `[gex] fork image: ${out.length} regions, ${(bytes / 1048576).toFixed(1)} MiB, `
        + `${(again / 1048576).toFixed(1)} MiB of it re-readable, `
        + `${Math.round(performance.now() - t0)}ms to copy`,
      );
    }
    return out;
  }

  /**
   * The memory map itself, whole: every region's name and the file its code
   * came from, which the three numbers of `gex_vma_at` cannot carry.
   *
   * A child that gets only spans calls every region `[forked]` and can
   * identify none of them, so it finds nothing in the shared module store and
   * lifts its parent's libraries again. In Chromium that was every process:
   * `hit=0`, `served=0`, eight thousand blocks per command, for ever.
   */
  #vmaBlob() {
    const e = this.#shim.exports;
    if (!e.gex_vma_blob_len || !e.gex_vma_blob_read) return null;
    const len = e.gex_vma_blob_len() >>> 0;
    if (!len) return null;
    const buf = e.gex_alloc(len) >>> 0;
    let blob = null;
    if (e.gex_vma_blob_read(buf, len) === len) {
      blob = new Uint8Array(this.#memory.buffer, buf, len).slice();
    }
    e.gex_free?.(buf, len);
    return blob;
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
  #kernelImports(shimMod) {
    const k = this.#kernel;
    const absent = () => 0;
    // What the shim actually imports, asked of the shim.
    //
    // This was a list written by hand, and the list is a third place to keep
    // in step with the other two. Adding `k_stdin_ready` to the kernel and to
    // the shim and not to the list cost a round: every run died with "Import
    // #80 gex_k k_stdin_ready: function import requires a callable", and
    // `check-glue.mjs` did not catch it because it checks this file against
    // the *shim's* imports and these arrive through the kernel.
    //
    // `WebAssembly.Module.imports` is the only authority on what a module
    // needs, so ask it. A name the kernel does not have still fails loudly
    // below; a name nobody imports costs nothing.
    const asked = shimMod
      ? WebAssembly.Module.imports(shimMod)
          .filter((i) => i.module === 'gex_k' && i.kind === 'function')
          .map((i) => i.name)
      : [];
    // Kept only for a caller that has no module to ask -- there is none in
    // this file, and this list is not to be extended.
    const LEGACY_NAMES = [
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
      // Which dispatch slots the code of a given identity is in, for the
      // whole system rather than for one Worker. Without a kernel these
      // answer nought, which is exactly right: a run of one process has no
      // sibling to share with, and the shim keeps its own map anyway.
      'k_slot_get', 'k_slot_put', 'k_slot_alloc', 'k_slot_floor',
      'k_stdin_write', 'k_stdin_read', 'k_stdin_ready', 'k_stdin_eof',
      'k_mod_claim_object', 'k_mod_for_object',
      'k_place', 'k_place_set',
      'k_mod_store', 'k_mod_claim', 'k_mod_for', 'k_mod_slots',
      'k_mod_len', 'k_mod_dirs_len', 'k_mod_read', 'k_mod_dirs_read',
    ];
    const NAMES = asked.length ? asked : LEGACY_NAMES;
    // Not imported by the shim -- the host reads it after a crash -- so it is
    // not in the list above and its absence is not a link error.
    // Two of these return an i64, which JS has to answer with a BigInt.
    const BIG = new Set(['k_wait', 'k_take_signals']);
    const out = { k_present: () => (k ? 1 : 0) };
    for (const n of NAMES) {
      // Some of what the shim imports from `gex_k` is answered here rather
      // than by the kernel -- `k_present` is the negotiation itself, and the
      // kernel has no such export. Deriving the list from the shim's imports
      // therefore turns up names this file already owns, and demanding them
      // of the kernel fails every run.
      if (n in out) continue;
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
   * Which object each translated page belongs to, as identities.
   *
   * The companion to `#pageMap`, and the one that matters after an `execve`:
   * the page map describes an address space that is about to be replaced,
   * this describes the libraries in it.
   */
  #sharedMap() {
    const e = this.#shim.exports;
    if (!e.gex_shared_count) return [];
    const n = e.gex_shared_count() >>> 0;
    if (!n) return [];
    const p = e.gex_alloc(n * 20) >>> 0;
    const wrote = e.gex_shared_dump(p, n * 20);
    const out = wrote > 0 ? this.#bytes().slice(p, p + wrote * 20) : new Uint8Array(0);
    e.gex_free?.(p, n * 20);
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
  async adoptProcess(shimWasm, guestWasm, child, opts = {}) {
    // Narrated when asked, because every step here is a place a child can
    // stop dead with nothing to show for it: a copy, two compiles, a kernel
    // and a table of code. A child that hangs is indistinguishable from one
    // that is merely slow to start, and it took a bisect to find out which.
    // Timed, not just narrated. Eighty-five processes at ten seconds of wall
    // clock each, translating half a second apiece, means the cost is in
    // getting a process *started* -- and every step here was a place that
    // could be spending it with nothing to say so.
    const began = performance.now();
    let last = began;
    const step = globalThis.GEX_DEBUG_EXIT
      ? (s) => {
          const now = performance.now();
          console.error(
            `[gex] adopt ${child.pid}: ${s} (+${Math.round(now - last)}ms, `
            + `${Math.round(now - began)}ms in)`,
          );
          last = now;
        }
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
    // A deferred child must not claim its parent's slots. See
    // `gex_reset_directory`: the inherited directory names one for every page
    // the parent translated, this child has filled none of them, and every
    // branch is then a bet on what the slot holds. Rebuilt rather than
    // erased, because the guest module's own entries came from a passive data
    // segment that the copied memory has already marked as applied, so they
    // are never written again and a child that loses them cannot run at all.
    if (adoptAfterTraps() > 0 && (child.modules?.length ?? 0) > 0 && e.gex_reset_directory) {
      step(`directory rebuilt: ${e.gex_reset_directory() >>> 0} entries kept`);
    }
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
    if (o.perMiss) e.gex_set_per_miss?.(o.perMiss);
    if (o.tty) e.gex_set_tty(1);
    if (o.uid !== undefined) e.gex_set_identity(o.uid, o.gid ?? o.uid);
    // The map, which the bytes do not carry: without it the child's first
    // `mmap` hands out an address its own image is already at.
    // Preferring the blob, which carries each region's name and the file its
    // code came from as well as its span. The three-number form stays for a
    // shim built before the blob existed.
    const blob = child.vmaBlob;
    if (blob && e.gex_adopt_vma_blob) {
      const p = e.gex_alloc(blob.length) >>> 0;
      new Uint8Array(this.#memory.buffer, p, blob.length).set(blob);
      e.gex_adopt_vma_blob(p, blob.length);
      e.gex_free?.(p, blob.length);
    } else {
      for (const r of child.image) {
        if (r.end !== undefined) e.gex_adopt_vma(BigInt(r.at), BigInt(r.end), r.prot);
      }
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
    // And the identity map, which is what is left of all this once the child
    // execs. Nothing reads it until a translation misses, and the miss path
    // checks each key against what this process has really mapped, so
    // seeding it early costs nothing and claims nothing.
    if (child.shared?.length && e.gex_shared_seed) {
      const n = (child.shared.byteLength / 20) | 0;
      const p = e.gex_alloc(n * 20) >>> 0;
      this.#bytes().set(child.shared.subarray(0, n * 20), p);
      e.gex_shared_seed(p, n);
      e.gex_free?.(p, n * 20);
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
    // The closure first: its slots sit below anything the parent translated,
    // and a child's dispatch tables start empty however full its parent's
    // were. The *directory* came with the adopted memory, so without this the
    // child's branches point at slots nothing has filled.
    this.#closureModules = opts?.closure ?? this.#closureModules;
    this.#wantLibrary = opts?.wantLibrary ?? this.#wantLibrary;
    await this.#installClosure();
    // Reachable, not filled.
    //
    // A child used to instantiate every module its parent had -- three
    // thousand of them for a shell that has been running a while, two to five
    // seconds of a browser's time -- and then, in almost every case,
    // `execve` something else and throw the entire image away. Every command
    // a shell runs was paying to wire up libraries it was about to discard.
    //
    // Growing the tables is the part that cannot be skipped: the directory
    // arrived with the adopted memory and names slots by number, and a branch
    // to a slot past the end of a table is a trap rather than a miss. Grown
    // slots hold the miss stub, so a branch into one asks for its code and
    // gets it from the shared store -- which is where the parent put it, and
    // where a fork is no longer the only way to find it.
    //
    // What this costs is a child that does *not* exec: it re-serves what it
    // touches instead of arriving with it. What it saves is every child that
    // does.
    // Only as far as the directory this child will actually dispatch through.
    //
    // A deferred child rebuilds the directory (`gex_reset_directory` above),
    // keeping only the image module's own entries -- so the parent's slot
    // total is not a bound on anything it can branch to, and growing to it is
    // 272ms of allocating and stub-filling 1.3 million table entries before
    // the child runs an instruction. Eighty-five processes in an `apt-get
    // update` is twenty-three seconds of it.
    //
    // Growing to the parent's total came from #114, on the theory that a
    // branch was running off the end of a table. It was not: `enter` was
    // failing to mask the table index out of a directory entry, and the
    // tables were never too small. The theory was wrong and this is what it
    // cost.
    //
    // A child that does *not* defer installs its parent's modules here and
    // each of those grows the tables to its own `need`, which is what the
    // maximum below is for.
    let need = 0;
    if (adoptAfterTraps() <= 0) {
      // Eager: every inherited module is installed below, so the tables have
      // to hold all of them.
      for (const m of child.modules ?? []) need = Math.max(need, m.need);
      need = Math.max(need, (child.slots ?? 0) >>> 0);
    }
    if (need) this.#growTables(need);
    // Kept, not dropped: a child that turns out to be staying wants them
    // after all, and by then its parent's Worker may be gone. See
    // `#maybeAdopt`.
    this.#inherited = child.modules ?? [];
    // Zero means the old behaviour: install them all here, before the child
    // runs a single instruction. Kept so the two can be compared without
    // rebuilding, because "deferred" and "eager" differ in more than timing
    // -- a deferred child starts with empty tables and meets its parent's
    // code through the miss path, which is a different code path entirely.
    if (adoptAfterTraps() <= 0) {
      step(`installing ${this.#inherited.length} modules`);
      const mine = this.#inherited;
      this.#inherited = [];
      for (const m of mine) await this.#adoptModule(m);
    } else {
      step(`${this.#inherited.length} modules reachable, ${need} slots`);
    }
    // Last of all, because everything above writes to this memory: growing it,
    // instantiating two modules against it, building a kernel in it and
    // installing a couple of thousand modules whose element segments the
    // engine applies. Any of those can land on the parent's bytes, and the
    // guest would meet the damage later and somewhere else.
    if (globalThis.GEX_DEBUG_EXIT && child.image?.[0]?.sum !== undefined) {
      const now = this.#bytes();
      let bad = 0;
      for (const r of child.image) {
        const end = r.at + r.bytes.byteLength;
        if (sum32(now.subarray(r.at, end)) === r.sum) continue;
        if (bad++ < 8) {
          console.error(
            `[adopt] region ${r.at.toString(16)}..${end.toString(16)} ` +
              `(${r.prot ?? '-'}) is not what the parent sent`,
          );
        }
      }
      step(bad ? `${bad} of ${child.image.length} regions differ` : 'image verified');
    }
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
      // The map, separately from the bytes it describes: names and code
      // identities, which the child needs to recognise a page as belonging to
      // a library somebody has already translated.
      vmaBlob: this.#vmaBlob(),
      // The map from a guest page to the dispatch slots its blocks are in,
      // and the modules those slots point into. Neither is in the memory --
      // the map is Rust data in this Worker's heap and the modules are
      // compiled wasm the engine holds -- so neither crosses with the bytes.
      //
      // A `WebAssembly.Module` is structured-cloneable, so handing the child
      // these costs nothing: it links them against its own memory and tables
      // without compiling a line.
      // Every slot this process has claimed, which is not the same as the
      // highest any of its modules needed.
      //
      // The child sizes its tables so that the directory it inherits cannot
      // name a slot past their end -- and the directory is written when a
      // page is *claimed*, not when its module is installed. Taking the
      // maximum over the inherited modules therefore misses anything claimed
      // after the last install: a translation still in the queue at `fork`,
      // or a page whose module went to another Worker of this process. A
      // branch into one of those is `table index is out of bounds`, which is
      // a trap rather than a miss, so it cannot be recovered from. See #114.
      slots: e.gex_jit_slots_total?.() ?? 0,
      pages: this.#pageMap(),
      // And which object each of those pages belongs to, which is the only
      // part of it that outlives the child's first `execve`. See
      // `gex_shared_dump`.
      shared: this.#sharedMap(),
      // What this process *holds*, not what it has instantiated.
      //
      // A child that defers takes its parent's modules without installing
      // them, so `#compiled` -- which only records installs -- stays empty
      // for its whole life. Forking from there handed the grandchild an
      // empty list and a slot count of zero, so it grew no dispatch tables
      // at all while inheriting a directory that names its grandparent's
      // slots. The chain has to carry what was inherited as well as what was
      // built, or deferring once breaks every descendant.
      modules: [...this.#inherited, ...this.#compiled],
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
          // "inside" only when the call is still on the stack. It used to say
          // "inside" for the last call the guest had *ever* made, which sent
          // this crash into `set_robust_list` -- a handler that returns zero
          // and reads nothing -- for three days. The guest had returned from
          // it and run on into translated code, which is where it died.
          const within = ask(e.gex_in_syscall, (v) => v) === 1;
          // `!0` reaches JavaScript as -1, not 0xffffffff: the export is a
          // `u32` in Rust and an `i32` in wasm, and there is no unsigned. The
          // sentinel check never matched, so a crash before the guest had made
          // any syscall at all reported "after syscall -1".
          const nr = ask(e.gex_last_syscall, (n) =>
            n === -1 || n === 0xffffffff
              ? ''
              : `, ${within ? 'inside' : 'after'} syscall ${n}`);
          if (globalThis.GEX_TRAP_STACK) console.error(err.stack ?? err);
          // "table index is out of bounds" says a branch reached a slot past
          // the end of the table it lives in, and nothing else in the message
          // says which slot or which table. Both are knowable here: the
          // directory entry for the failing pc holds the packed slot, and the
          // tables can be measured. Without this the only way to tell a slot
          // that was never grown to from a slot that was never filled is to
          // guess, and they have completely different causes.
          // The address `enter` was *called with*, not the one in the saved
          // context.
          //
          // `enter(ctx, pc)` dispatches on its argument, so when it traps on
          // a table the entry that failed is that page's -- and the context's
          // PC field is whatever the guest last wrote there, which after an
          // `execve` or a thread switch is a different address entirely. Two
          // rounds of this hunt were spent on entries that were in range
          // because they belonged to a page that was not the one being
          // dispatched.
          // The target of the last indirect branch, in preference to
          // anything else.
          //
          // The dispatch ends in `return_call_indirect`, so blocks replace
          // each other's frames: by the time this runs, `enter` is long gone,
          // the engine's stack has no wasm frames at all, and every address
          // still obtainable -- the argument `enter` was called with, the PC
          // in the saved context -- belongs to somewhere the guest has
          // already left. Three crash reports were written against three such
          // addresses. This one is stored by the dispatch itself, immediately
          // before the branch.
          const target = ask(e.gex_last_target, (v) => v) || 0n;
          const where = ask(() => (target ? target : pc), (entered) => {
            if (!/table index/.test(err.message)) return '';
            const raw = BigInt(entered);
            const guest = raw < 0n ? raw + (1n << 64n) : raw;
            const page = Number(guest >> 12n);
            const dir = new DataView(this.#memory.buffer).getUint32(DISPATCH_DIR + page * 4, true);
            const tables = this.#dispatchTables();
            const packed = dir >>> 0;
            const idx = packed >>> 24;
            const slot = packed & 0xffffff;
            // The page's base is not the index the branch uses: the block's
            // offset within the page is added to it. Reporting the base alone
            // showed a slot comfortably inside the table while the branch was
            // past its end, which is a diagnosis pointing at the wrong thing.
            const at = slot + ((Number(guest) & 0xfff) >> 2);
            const len = tables[idx]?.length ?? 0;
            // Asked, not computed. Every report so far has worked out an
            // index, compared it against a length, found it comfortably
            // inside and said so -- while the engine insisted the same
            // dispatch was out of bounds. One of the two is wrong about which
            // table this is, and `get` settles it: a `RangeError` means the
            // table really is short and the length here is not the length the
            // branch met; a function back means the branch would have worked
            // and the trap belongs to some other table operation entirely.
            let probe;
            try {
              probe = tables[idx].get(at) ? 'holds a function' : 'is empty';
            } catch (e) {
              probe = `cannot be read: ${e.message}`;
            }
            // And when that entry is fine, the whole directory.
            //
            // A dispatch is for the branch's *target*, not for the address
            // the guest is standing on, so the entry at `dispatch_page(pc)`
            // is usually a different entry from the one that trapped -- which
            // is why reporting only that one said "in range, so the trap is
            // elsewhere" and stopped. Every entry is checkable against the
            // tables this process actually has, and an unusable one is the
            // answer whether or not the pc happens to name it.
            //
            // Tables 2 to 7 are typically length zero, so an entry whose top
            // eight bits decode to a table that high traps on its first use
            // however small its slot. Those are worth separating from an
            // entry naming a real table past its end: one is a corrupt or
            // stale directory write, the other is a table that was not grown.
            let bad = 0;
            let highTable = 0;
            const sample = [];
            const dv = new DataView(this.#memory.buffer);
            for (let p = 0; p < 0x80000; p++) {
              const v = dv.getUint32(DISPATCH_DIR + p * 4, true) >>> 0;
              if (v === 0) continue;
              const t = v >>> 24;
              const b = v & 0xffffff;
              const cap = tables[t]?.length ?? 0;
              // The whole page, because any block in it may be branched to.
              if (t < tables.length && b + 1024 <= cap) continue;
              bad++;
              if (t >= 2) highTable++;
              if (sample.length < 6) {
                sample.push(`page ${p} -> ${t}[${b}] (table is ${cap})`);
              }
            }
            const survey = bad
              ? `. ${bad} directory entries name a slot this process cannot ` +
                `reach, ${highTable} of them in a table above 1: ${sample.join('; ')}`
              : '. every other directory entry is reachable';
            return (
              `. the last indirect branch was to 0x${guest.toString(16)}` +
              `${target ? '' : ' (no target recorded, so this is where enter was called)'}` +
              `, page ${page} of the directory says table ${idx} slot ${slot}` +
              `, so this branch wants ${idx}[${at}] and that table is ${len} long` +
              ` (${at < len ? 'in range' : 'past the end'}, and reading it says it ${probe})` +
              survey +
              `, and this process has ${tables.length} tables of ` +
              `[${tables.map((t) => t.length).join(', ')}]` +
              `, ${this.#installs} modules installed`
            );
          });
          throw new Error(
            `the gex runtime trapped inside the shim (${err.message}${at}${nr}` +
              `${code}${heap}${where}). That is a crash in the runtime, not a guest exit; ` +
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
        // A slot the shim wants written before the guest goes back to it.
        //
        // Its module is instantiated and its segments are passive, so the
        // code is present and simply not in the table yet. Filling here costs
        // one segment; filling at instantiation cost every block of the
        // object, in every process.
        const want = e.gex_fill_wanted ? Number(e.gex_fill_wanted()) : -1;
        if (want >= 0) {
          // Whether anybody answered matters: a page nobody holds *yet* must
          // be askable again once another module arrives, and a page nobody
          // will ever hold must stop trapping. The shim cannot tell those
          // apart on its own.
          const quick = this.fillSlotNow(want >>> 0);
          e.gex_fill_done((quick ?? (await this.fillSlot(want >>> 0))) ? 1 : 0);
        }
        // One grow for the whole queue, before any of it is installed.
        //
        // `#installModule` grows to its own module's need, and `Table.grow`
        // reallocates -- so a batch of four hundred modules with a rising
        // need grows four hundred times, each copying a larger table. When
        // the miss path started serving a finished object whole, that turned
        // 317ms of growing into 1741ms and swallowed the compile it saved.
        if (e.gex_jit_pending_len() >>> 0) {
          const need = (e.gex_jit_pending_slots?.() ?? 0) >>> 0;
          if (need) this.#growTables(need);
        }
        while (e.gex_jit_pending_len() >>> 0) await this.#installTranslation();
        // The parent's modules go in *after* this Worker's own queue, never
        // between a translation and its install.
        //
        // The comment above is the reason: a module's page assignments are
        // the next one's starting point, and the shim hands them out in the
        // order the modules were emitted. Bulk-installing two thousand
        // inherited modules while a freshly translated one was still queued
        // interleaved two orderings, and the process that hit it faulted on
        // guest memory inside a syscall several forks later.
        await this.#maybeAdopt();
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
            // Every thread is waiting and nothing woke them.
            //
            // Said out loud, and never as a success. This used to return
            // `gex_exit_code()` quietly, and for a guest that had not exited
            // that code is zero -- so a run that stopped for a reason nobody
            // here recognised was reported as a program that finished
            // perfectly. `apt-get --version` "succeeded" that way, printing
            // nothing, and so did every other thing that stopped here; the
            // failure was invisible for as long as it has existed.
            //
            // A guest that really exited has said so, and that code is its
            // own. Anything else is this runtime giving up, which is 125 --
            // outside the range a program of its own accord would use.
            const stalled = e.gex_exited ? !e.gex_exited() : false;
            const why =
              `[gex] the run stopped and nothing could wake it ` +
              `(wake=${woke}, exited=${!stalled})`;
            this.#env.write(2, new TextEncoder().encode(`${why}\n`));
            console.error(why);
            return stalled ? 125 : e.gex_exit_code();
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
              // `exited` or `ABANDONED`, because `unreachable` and code 0
              // are what both a finished program and a crashed shim produce.
              // `gex_exited` has known the difference all along and this line
              // did not print it.
              `[gex] run ended: ${e.gex_exited?.() ? 'exited' : 'ABANDONED'}`
              + `, code ${e.gex_exit_code()}${where}${heap}`,
            );
            // And what the global kernel has to say, which is where the
            // answer has been sitting.
            //
            // That module imports two memories and nothing else, so it has no
            // console: its panic hook writes the message into its own memory
            // for a host to read. `worker.mjs` reads it -- but only on the
            // path where the error *propagates*, and a guest that abandons
            // the stack does not take that path. So a kernel panic during a
            // syscall reported as `ABANDONED, in syscall 57` and nothing
            // else, with the reason recorded and unread a function call away.
            if (!e.gex_exited?.()) {
              try {
                const k = this.#kernel;
                const n = k?.k_panic_len?.() >>> 0;
                if (n) {
                  const at = k.k_panic_ptr() >>> 0;
                  const mem = this.#kernelSpec.memory;
                  console.error(
                    '[gex] the global kernel panicked: '
                    + new TextDecoder().decode(new Uint8Array(mem.buffer, at, n)),
                  );
                }
              } catch {
                // A crash report must not crash.
              }
            }
            // And the file it was asking for, when it died inside `openat`.
            if (!e.gex_exited?.() && e.gex_last_open && e.gex_alloc) {
              try {
                const max = 256;
                const p = e.gex_alloc(max) >>> 0;
                const n = e.gex_last_open(p, max) >>> 0;
                const path = new TextDecoder().decode(this.#bytes().subarray(p, p + n));
                e.gex_free?.(p, max);
                if (path) console.error(`[gex] the last path it opened: ${path}`);
              } catch {
                // A crash report must not crash.
              }
            }
            // And the wasm stack, when nobody set a status.
            //
            // A program that finished throws `unreachable` on purpose and its
            // stack says nothing worth reading. A shim that faulted throws the
            // same thing, and its stack names the function that did it --
            // which is the one fact that turns "dpkg abandoned the stack
            // inside openat" into a line of Rust. It was being thrown away on
            // exactly the path that needed it.
            if (!e.gex_exited?.()) {
              console.error(`[gex] the stack under it:\n${err.stack ?? String(err)}`);
            }
          }
          return e.gex_exit_code();
        }
        throw err;
      }
    }
  }

  /**
   * Make every dispatch table long enough to hold `need` slots, and return
   * them.
   *
   * Separated from installing a module because growing is the cheap half and
   * the two are needed apart. A forked child has to be able to *reach* every
   * slot its parent's directory names -- otherwise a branch into one is a
   * trap, not a miss -- without instantiating the modules that fill them.
   *
   * An unfilled slot holds the miss stub, so a branch into one asks the host
   * for the code instead of finding a hole. That is what makes growing
   * without filling safe, and it is why the stub is what a grown table is
   * filled with rather than null.
   */
  #growTables(need) {
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
    // Each table's own need, when the shim can say. One running total across
    // the whole virtual slot space cannot express "table 0 wants 400,000 and
    // table 4 wants 2,048" -- read as a total that is table 4's base plus
    // 2,048, and the clamp below grows tables 0..3 to ten million each.
    // Which table the highest slot this module writes falls in, and how far
    // into it. Slot indices are global and the mapping is arithmetic --
    // table `slot / per`, offset `slot % per` -- so a slot in table 4 does
    // not require tables 0 to 3 to be full. Only the table it lands in has
    // to be long enough.
    const top = need > 0 ? need - 1 : -1;
    const topTable = top >= 0 ? Math.floor(top / per) : -1;
    const topWants = top >= 0 ? (top % per) + 1 : 0;
    // The shim's per-table high-water, *and* what the module about to be
    // instantiated reaches. Neither alone is enough.
    //
    // `gex_table_need` counts only the slots this shim allocated in this
    // process. A closure library's slots were allocated by `gexc` at build
    // time and a forked child's by its parent, so a module handed in from
    // JavaScript is invisible to it: table 4 reads 0 while the module's
    // element segments write at 39,998,464, and instantiating answers
    // "table index is out of bounds" with
    // `tables are [2886656, 0, 0, 0, 0, 0, 0, 0]`. That is `apt-key`'s
    // child, and it ends the run.
    //
    // The old fallback is not the answer either: read as a running total,
    // 39,998,464 asks for tables 0 to 3 at ten million entries each, which
    // is 320 MB of table for a module that wants 1,024 slots in table 4.
    const need_of = e.gex_table_need
      ? (i) => Math.max(e.gex_table_need(i) >>> 0, i === topTable ? topWants : 0)
      : (i) => Math.min(Math.max(need - i * per, 0), per);
    // Table 0's slot 0 is the miss stub, and it is what an unfilled slot in
    // any table must hold: a branch to an address nothing has translated has
    // to reach the host, not a stale function. Tables past the first start
    // empty, so they have no element 0 of their own to grow from.
    const stub = tables[0].get(0);
    for (let i = 0; i < tables.length; i++) {
      const need_i = need_of(i);
      if (tables[i].length >= need_i) continue;
      // Geometrically, not exactly.
      //
      // `Table.grow` reallocates and copies, so growing by what one module
      // happens to want means copying an ever-larger table once per module.
      // An `apt-key` process installs two hundred, and this went from 1ms to
      // 747ms of a 1047ms install -- more than compiling and instantiating
      // them put together. The closure path already knew this and grew once
      // after its loop; the runtime path did not, and could not, because it
      // meets its modules one at a time.
      //
      // Doubling makes the total copying linear in the final size. The cost
      // is up to twice the table in address space, which is eight bytes a
      // slot of a `WebAssembly.Table` that is mostly the miss stub.
      const want = Math.min(Math.max(need_i, tables[i].length * 2), per);
      try {
        tables[i].grow(want - tables[i].length, stub);
      } catch (err) {
        throw new Error(
          `dispatch table ${i} would not grow from ${tables[i].length} to ${want}: ${err.message}`,
        );
      }
    }
    return tables;
  }

  /**
   * Take the parent's modules after all, if this child turns out to be
   * staying.
   *
   * A forked child is handed every module its parent had. Instantiating them
   * costs two to five seconds -- 924ms of every 933 is `instantiate`, because
   * a module carries its blocks in element segments -- and almost every child
   * a shell forks spends that and then `execve`s something else, throwing the
   * whole image away. So a child now arrives with its tables *grown* and
   * nothing filled: an unfilled slot holds the miss stub, so a branch asks
   * for its code rather than trapping, and a child that execs pays nothing.
   *
   * The other kind of child is the reason for this method. One that keeps
   * running its parent's code meets it a miss at a time, and there are
   * thousands of pages; past a few dozen misses it is plainly not about to
   * exec, and taking the parent's modules in one go is cheaper than
   * continuing to fault. The threshold is deliberately low -- a child that
   * execs takes only a handful of misses on the way there, in the few
   * hundred instructions between `fork` returning and `execve`.
   */
  async #maybeAdopt() {
    if (!this.#inherited.length) return;
    if (++this.#traps < adoptAfterTraps()) return;
    const mine = this.#inherited;
    this.#inherited = [];
    for (const m of mine) await this.#adoptModule(m);
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
  async #installModule(bytes, need, segments = 64) {
    const t0 = performance.now();
    this.#installs++;
    try {
      return await this.#installModuleInner(bytes, need, segments);
    } finally {
      this.#installMs += performance.now() - t0;
    }
  }

  /**
   * The install itself, timed by the wrapper above.
   *
   * Timed because the counter this project steers by -- `millis translating`
   * -- measures only the lifting, and lifting turned out not to be what a run
   * costs. The second `apt-get update` in one browser lifts for 0.4 seconds
   * across ten processes and still takes 69, and each of those processes
   * installs upward of two thousand shared modules. Compiling and
   * instantiating those is work no existing counter could see.
   */
  async #installModuleInner(bytes, need, segments = 64) {
    const t0 = performance.now();
    const e = this.#shim.exports;
    const tg = performance.now();
    const tables = this.#growTables(need);
    this.#growMs += performance.now() - tg;
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
    const tc = performance.now();
    const mod = bytes instanceof WebAssembly.Module ? bytes : await WebAssembly.compile(bytes);
    this.#compileMs += performance.now() - tc;
    // With its metadata, when there is any to read.
    //
    // A fork hands the child `#compiled`, and until now that was `{mod, need}`
    // -- enough to instantiate and nothing else. The child therefore had to
    // instantiate all of them, about 485 in an `apt-get update`, because it
    // had no way to know which module covered which page. The metadata is
    // sitting in the bytes as a custom section and costs a parse to keep.
    this.#compiled.push({
      mod,
      need,
      meta: bytes instanceof Uint8Array ? metadataSection(bytes) : undefined,
    });
    const imports = { ...this.#imports };
    for (let i = 0; i < tables.length; i++) imports[TABLE_NAMES[i]] = tables[i];
    // Instantiating is where a module's blocks reach the tables: it carries
    // element segments that write them, so a slot past the end of a table is
    // a trap *here* rather than at the branch that would have used it. The
    // engine's message says only "table index is out of bounds", which reads
    // like a guest dispatch and is not one, so the numbers that distinguish
    // them are attached here.
    let inst;
    const ti = performance.now();
    try {
      inst = await WebAssembly.instantiate(mod, { gex: imports });
      this.#instantiateMs += performance.now() - ti;
    } catch (err) {
      throw new Error(
        `installing a module of ${need} slots failed: ${err.message}. ` +
          `tables are [${tables.map((t) => t.length).join(', ')}], ` +
          `${(e.gex_slots_per_table?.() ?? 0) >>> 0} slots per table, ` +
          `${this.#installs} modules in so far`,
      );
    }
    // Write the module's blocks into the tables.
    //
    // Its element segments are passive, so instantiating wrote nothing. That
    // is deliberate: a module is instantiated once per guest *process* --
    // a table belongs to an instance and every process has its own -- and an
    // active segment makes every process write a slot for every block the
    // module holds. For a closure library that is every block of a whole
    // object: `libgcrypt` is 303 MiB of wasm, and instantiating the closure
    // cost between 1.2 and 2.2 seconds a process, nearly all of it slots for
    // code that process never reached.
    //
    // Every segment is filled here, which is the same work as before, only
    // moved out of instantiation and under the host's control. That control
    // is the point: the next step is to fill the segment covering a page
    // when a branch first reaches it, and never to fill the rest.
    //
    // `gexfill` is absent from a module built before this existed, and from
    // the image module, whose table is its own and still active.
    if (globalThis.GEX_DEBUG_EXIT && this.#installs <= 2) {
      console.error(`[fill] module ${this.#installs}: gexfill ${typeof inst.exports[FILL]}, exports ${Object.keys(inst.exports).slice(0, 4).join(',')}`);
    }
    // Written now, or when a branch asks for them.
    //
    // The module decides, and only a module built passive exports
    // `gexfillat`. Nothing else can decide it correctly: a host rule that
    // said "defer a closure install" left a forked child -- which inherits
    // its parent's modules through another path entirely -- filling the same
    // libraries eagerly, so nineteen of twenty processes had an empty
    // deferred list while every miss they took still asked. Answered 10,
    // unanswered 30, and 407,588 blocks translated against 17,462.
    //
    // A unit the runtime translator emits is below the block gate, has active
    // segments and no helpers, and so is never deferred by this test. That is
    // the right answer for it too: it exists *because* a branch missed on it,
    // so deferring it would only guarantee a second miss on the address that
    // caused the first.
    //
    // A big module is the whole reason the segments are passive: it is
    // instantiated once per guest process, and writing every block of it into
    // every process's table is 1.2 to 2.2 seconds of an apt run, nearly all
    // of it slots for code that process never reaches. A small unit is wanted
    // immediately and costs nothing, so it is written here and forgotten.
    if (typeof inst.exports[FILL_AT] === 'function') {
      this.#unfilled.unshift(inst);
      // And tell the shim there is now something worth asking for. Until a
      // module is deferred, the miss path must not spend a trap per page
      // asking a host that has nothing to give: that cost `apt-get update`
      // 114 seconds against 82.
      e.gex_fill_possible?.(1);
    } else if (typeof inst.exports[FILL_ALL] === 'function') {
      // One crossing, not one per segment. A module has as many as 355, and
      // calling in for each cost apt 30 seconds against instantiation doing
      // the same writes inside the engine.
      inst.exports[FILL_ALL]();
    } else if (typeof inst.exports[FILL] === 'function') {
      // Until it says there are no more. The count is the module's own: the
      // image module has one segment and libc's closure module has 355, and a
      // host that guessed sixty-four left 291 of them unwritten -- the guest
      // trapped on `unreachable` the first time it branched into one.
      for (let seg = 0; inst.exports[FILL](seg); seg++);
    }
    // Nothing to wire up below, for a module this translator built.
    //
    // Worth stating because it was measured rather than assumed, and because
    // it decides which optimisation is worth doing. A translated module
    // exports one function, `enter`, and installs its blocks into the
    // imported tables through its own element segments -- so the table is
    // filled by `instantiate` itself, and the loop below writes nothing. Of
    // 933ms of installing, 924 were `instantiate`.
    //
    // Which means the cost is per *module* and not per block: merging many
    // modules into one removes it, and anything that makes each module
    // cheaper to wire up cannot, because that part is already free. The loop
    // stays for modules of the older shape, which name each block.
    for (const name of Object.keys(inst.exports)) {
      if (name[0] !== 'b') continue;
      // The shim packs the table index into the top eight bits, the same way
      // a directory entry does.
      const packed = e.gex_dispatch_slot(BigInt('0x' + name.slice(1)));
      if (packed < 0) continue;
      tables[packed >>> 24].set(packed & 0xffffff, inst.exports[name]);
    }
    // The instance, so the caller can remember which library it belongs to.
    // With an image per program, "which module may answer for this page" has
    // an answer and the fill path needs it by name rather than by search.
    return inst;
  }

  /**
   * Let a module that is already instantiated answer fills.
   *
   * `#installModule` does this for every module it installs, and the *image*
   * module does not go through it: it is the guest module itself, built with
   * `Linkage::Own`, instantiated by `load` because it owns the memory and the
   * tables everything else imports. So its element segments -- passive, like
   * any module past the block gate -- were never written by anybody, and a
   * page's slots stayed on the miss stub while the module holding its blocks
   * sat instantiated in the same process.
   *
   * `bash` translated whole is 211,979 blocks and 42 MB of module, and with
   * it in place pid 1 lifted exactly as many blocks as without: 15,351, to
   * the block. A module nothing can fill from is a module that does nothing.
   */
  #offerToFill(inst) {
    if (typeof inst?.exports?.[FILL_AT] !== 'function') return;
    this.#unfilled.unshift(inst);
    this.#shim.exports.gex_fill_possible?.(1);
  }

  /**
   * Put a registered closure library into the directory, and grow for it.
   *
   * The pair that has to happen together: an entry names a slot, and a branch
   * to a slot past the end of a table traps rather than missing. Doing both
   * for every library at registration is what cost each process the whole
   * closure's slots; doing them here costs a process only the libraries it
   * enters.
   */
  #publish(lazy, i) {
    const e = this.#shim.exports;
    if (!lazy.meta || !lazy.meta.length || !e.gex_publish_closure_module) return;
    const t0 = performance.now();
    const gp = e.gex_alloc(lazy.meta.length) >>> 0;
    this.#bytes().set(lazy.meta, gp);
    e.gex_publish_closure_module(gp, lazy.meta.length, i);
    e.gex_free?.(gp, lazy.meta.length);
    this.#growTables(lazy.need);
    if (globalThis.GEX_DEBUG_EXIT) {
      console.error(
        `[closure] library ${i} published in ${Math.round(performance.now() - t0)}ms`,
      );
    }
  }

  /**
   * Write the slots around `slot`, from whichever unfilled module holds them.
   *
   * Answers whether anybody did. A module that has never been asked for a
   * slot it holds is the point of the exercise -- 303 MiB of `libgcrypt`
   * whose blocks this process will never branch into stays unwritten.
   *
   * Newest first, because a process that has just installed a module is
   * usually about to branch into it.
   */
  /**
   * Fill a page without waiting, or answer `null` if that is impossible.
   *
   * The common case by a wide margin: the page belongs to a library this
   * process has already built, and writing its slots is a call into wasm.
   * The `await` it replaces was not free -- it yields the Worker's event loop
   * with the guest stopped, and a warm `ls` asks 129 times, which was 1.8 of
   * its 2.0 seconds. Removing every block of translation from that run did
   * not move it; this is what the time was.
   *
   * Below `INTERP_BASE` the owner is consulted and nobody else, ever. Every
   * image in a closure sits at `PIE_BASE` and `gexfillat` matches on the
   * guest page, so a scan there hands `ls` whichever image answers first --
   * `bash`'s, in practice. Above it, page numbers are unique to an object and
   * a scan is safe: that is where runtime units and inherited modules live.
   */
  fillSlotNow(page) {
    const e = this.#shim.exports;
    const owner = e.gex_closure_owner ? e.gex_closure_owner(BigInt(page)) : -1;
    if (owner >= 0) {
      const lazy = this.#lazyClosure[owner];
      if (!lazy) return null;
      // Publishing is synchronous -- a directory write and a table grow --
      // so it belongs on this side of the fence.
      if (!lazy.published) {
        lazy.published = true;
        this.#publish(lazy, owner);
      }
      if (!lazy.instObj) return null;
      // A module below the block gate has *active* segments and no fill
      // helper: instantiating it wrote its slots, so there is nothing to ask
      // for and the page is already answered.
      if (typeof lazy.instObj.exports[FILL_AT] !== 'function') return true;
      if (lazy.instObj.exports[FILL_AT](page)) {
        this.#filled++;
        return true;
      }
      return null;
    }
    if (page * 4096 < 0x0300_0000) return null;
    for (const inst of this.#unfilled) {
      if (inst.exports[FILL_AT](page)) {
        this.#filled++;
        return true;
      }
    }
    return null;
  }

  async fillSlot(slot) {
    // The library that covers this page, instantiated now if it has not been.
    //
    // The shim answers by page because that is what a miss knows, and its
    // ordinals are the order the libraries were registered -- the same order
    // they went into `#lazyClosure`.
    const owner = this.#shim.exports.gex_closure_owner
      ? this.#shim.exports.gex_closure_owner(BigInt(slot))
      : -1;
    if (owner >= 0) {
      const lazy = this.#lazyClosure[owner];
      if (lazy && !lazy.published) {
        lazy.published = true;
        this.#publish(lazy, owner);
      }
      if (lazy && !lazy.inst) {
        lazy.inst = true;
        // The compiled module, fetched now if registration only brought its
        // metadata. A Worker is sent thirty-three metadata blobs at once --
        // a few kilobytes -- and asks for a module when a branch lands in it.
        const mod = lazy.mod ?? (this.#wantLibrary ? await this.#wantLibrary(owner) : null);
        if (mod) {
          lazy.mod = mod;
          // Remembered by index. `fillSlotNow` addresses a library by name
          // rather than searching, which is what makes the page below
          // `INTERP_BASE` answerable by its own image and no other.
          lazy.instObj = await this.#installModule(mod, lazy.need);
        } else {
          // Nothing to install; let the miss fall through and be lifted
          // rather than wedging on a library that never arrives.
          lazy.inst = false;
        }
      }
    }
    // And the same for a module this process inherited from its parent.
    //
    // These are ordinary runtime units: their element segments are active, so
    // instantiating writes their slots and there is nothing further to fill.
    // Asking is still worth it -- the alternative is the miss path serving or
    // lifting code this process is already holding.
    const heir = this.#shim.exports.gex_lazy_owner
      ? this.#shim.exports.gex_lazy_owner(BigInt(slot))
      : -1;
    if (heir >= 0) {
      const lazy = this.#lazyInherited[heir];
      if (lazy && !lazy.inst) {
        lazy.inst = true;
        await this.#installModule(lazy.mod, lazy.need);
        return true;
      }
    }
    // The owner first, and below `INTERP_BASE` the owner only: those pages
    // belong to every image at once and only one of them is ours.
    const mine = owner >= 0 ? this.#lazyClosure[owner]?.instObj : null;
    if (mine && typeof mine.exports[FILL_AT] !== 'function') return true;
    if (mine && mine.exports[FILL_AT](slot)) {
      this.#filled++;
      return true;
    }
    if (slot * 4096 >= 0x0300_0000) {
      for (const inst of this.#unfilled) {
        if (inst.exports[FILL_AT](slot)) {
          this.#filled++;
          return true;
        }
      }
    }
    // A request nobody can answer is the interesting one, and there should be
    // almost none: the shim only asks about a page it already has a base for,
    // which means a module it holds covers that code. If these are common the
    // deferred set is wrong -- a module was installed somewhere its slots are
    // wanted and not kept, or a forked child inherited a directory naming
    // pages no module of its own can fill. Capped, because a flood of them
    // would be the failure rather than the diagnosis.
    if (this.#unanswered++ < 24) {
      console.error(
        `[fill] nothing holds entry ${slot >>> 0}: ` +
          `${this.#unfilled.length} deferred modules, ${this.#filled} filled`,
      );
    }
    return false;
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
