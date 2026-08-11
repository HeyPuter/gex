// The page's half of a guest system: one Worker per guest process.
//
// This is what replaced the snapshot switcher. That switcher existed because
// the page had one wasm stack and one address space, so running a second
// process meant writing the first one out and reading the second one in --
// 223 MiB per switch during the gears demo. A Worker per process removes the
// cause rather than the cost: each has a `WebAssembly.Memory` of its own, so
// nothing collides and nothing is snapshotted.
//
// What the host has to do is small, and it is all here:
//
//   * make the first Worker and give it a program,
//   * make another whenever a Worker says its guest forked, and hand that one
//     the record the fork produced,
//   * carry output out and input in,
//   * hold the run open until the *first* process exits, which is the one the
//     caller asked for.
//
// Everything the processes share -- the process table, the filesystem, the
// pipes -- is in the global kernel, a second wasm module every Worker
// instantiates against one shared memory. None of it passes through here.
// That is deliberate: a syscall that had to be a message to this file and a
// wait for the answer would be slower than the switcher it replaced.

/**
 * A guest system: one process, and whatever it forks.
 *
 * `makeWorker` is the only part that differs between a page and Node, and it
 * is a callback rather than a branch because this file has no business
 * knowing which it is running under.
 */
export class ProcessHost {
  #makeWorker; #shim; #guest; #kernel; #extra;
  #onOutput; #onFork; #onExit; #readStdin; #onOther;
  #workers = new Map();
  #pending = new Map();
  #first = null;
  #forked = 0;
  /// When each process was asked for. See `#spawn`.
  #asked = new Map();
  #threads = 0;
  // The global kernel's per-Worker stacks, as the first Worker reported them,
  // and how many have been handed out.
  //
  // A wasm global belongs to an instance, so every Worker's kernel instance
  // has a `__stack_pointer` of its own -- all pointing at the same address in
  // the memory they share. Handing each one its own slice is what stops two
  // Workers in the kernel writing their locals over each other.
  #stacks = null;
  #stacksTaken = 0;
  // Slices handed back by Workers that have ended, and which slice each live
  // Worker holds. A run that starts a hundred short-lived threads uses as
  // many slices as it has threads *at once*, not as many as it ever made --
  // without which a pthread program exhausted the array while it was still
  // starting up.
  #stacksFree = [];
  /// The shim compiling, once, so no Worker waits on a second compile.
  #shimCompiling = null;
  #stackOf = new Map();

  constructor({
    makeWorker,
    shim,
    guest,
    kernel = null,
    // Merged into every Worker's start message. Everything a process needs
    // that is not the program itself: the GPU channel, the proxy's address,
    // the terminal's size. Every process gets the same, because on a real
    // machine every process does.
    extra = {},
    onOutput = () => {},
    onFork = () => {},
    // Every process's exit message, whole. `start` resolves with pid 1's
    // status, which is what a caller usually wants; this is for the rest of
    // what a Worker reports on its way out -- how long its guest ran, what
    // its memory grew to, anything read out of its address space before the
    // thread that owns it goes away.
    onExit = () => {},
    readStdin = null,
    // Anything a Worker posts that this file has no meaning for -- the GPU
    // asking to be served, a network event for the page's status line.
    onMessage = () => {},
  }) {
    this.#makeWorker = makeWorker;
    this.#shim = shim;
    this.#guest = guest;
    this.#kernel = kernel;
    this.#onOutput = onOutput;
    this.#onFork = onFork;
    this.#onExit = onExit;
    this.#readStdin = readStdin;
    this.#extra = extra;
    this.#onOther = onMessage;
  }

  /**
   * The shim, compiled once for every Worker that will ever need it.
   *
   * Two things this saves, and it is worth being clear that only the second
   * is certain. V8 keeps a cache of compiled wasm keyed on the wire bytes and
   * shares it between Isolates in a process, so ten Workers compiling the
   * same 8.7 MB may already have been one compilation and nine lookups --
   * that is measurable, and measured below rather than assumed. What is not
   * in doubt is the copying: `postMessage` structured-clones a `Uint8Array`,
   * so every Worker was handed its own 8.7 MB, while a `WebAssembly.Module`
   * clones as a reference to the code already compiled.
   */
  #shimModule() {
    if (this.#shim instanceof WebAssembly.Module) return Promise.resolve(this.#shim);
    // Asynchronously, and this is not a preference.
    //
    // `new WebAssembly.Module(bytes)` compiles synchronously, and Chromium
    // refuses to do that on the main thread for a buffer over 8 MB -- the
    // shim is 8.7. Node has no such rule, so the synchronous form passed
    // every test here and killed the page at boot with `RangeError:
    // WebAssembly.Compile is disallowed on the main thread`. The one
    // instrument that caught it was an `unhandledrejection` handler added
    // minutes earlier; before that the page simply stopped and the driver
    // waited.
    this.#shimCompiling ??= WebAssembly.compile(this.#shim).then((m) => (this.#shim = m));
    return this.#shimCompiling;
  }

  /** How many children the guest has forked. */
  get forked() { return this.#forked; }

  /** How many threads its processes have started in Workers of their own. */
  get threads() { return this.#threads; }

  /** Every live process, by pid. Pid 1 is the one `start` was given. */
  get pids() { return [...this.#workers.keys()]; }

  /**
   * Run `elf` as pid 1 and resolve with its exit status.
   *
   * Children outlive it only as long as it takes to stop them: a page that
   * has finished with a program has finished with the daemons it started,
   * and leaving a Worker running would leave the guest's clock ticking in a
   * tab nobody is looking at.
   */
  async start({ elf, files = {}, args = [], opts = {} }) {
    const status = await this.#spawn(1, {
      kind: 'run',
      elf,
      files,
      args,
      opts,
    });
    this.stop();
    return status;
  }

  /** Deliver a line of standard input to pid 1. */
  pushStdin(bytes) {
    this.#first?.postMessage({ kind: 'stdin', bytes });
  }

  /** Say there will be no more input. */
  endStdin() {
    this.#first?.postMessage({ kind: 'eof' });
  }

  /** Tell every process the terminal changed size. */
  resize(rows, cols) {
    for (const w of this.#workers.values()) w.postMessage({ kind: 'winsize', rows, cols });
  }

  /** Stop every Worker, whatever it is doing. */
  stop() {
    for (const w of this.#workers.values()) w.terminate();
    this.#workers.clear();
  }

  /**
   * One process in one Worker.
   *
   * `msg` is either `run` -- load a program -- or `adopt`, which is what a
   * forked child gets: a copy of its parent's memory and the few words that
   * differ between the two.
   */
  #spawn(pid, msg) {
    // When this process was asked for, so the gap between asking and it
    // running can be seen.
    //
    // Everything measured so far is what a process does *once it is up*:
    // translating, installing, adopting. Those add to one or two seconds and
    // an `apt-get update` spends nine per process, so most of a process's
    // life is in none of them -- and nothing timed the part in between:
    // making a Worker, loading the bundle into it, and getting fifteen
    // megabytes of image across.
    this.#asked.set(pid, performance.now());
    return new Promise((resolve, reject) => {
      const w = this.#makeWorker();
      this.#workers.set(pid, w);
      if (pid === 1) this.#first = w;
      w.addEventListener?.('message', (e) => this.#onMessage(pid, w, e.data, resolve, reject));
      w.on?.('message', (m) => this.#onMessage(pid, w, m, resolve, reject));
      const fail = (e) => {
        this.#workers.delete(pid);
        reject(e instanceof Error ? e : new Error(e?.message ?? String(e)));
      };
      w.addEventListener?.('error', fail);
      w.on?.('error', fail);
      this.#pending.set(pid, { ...msg, pid });
    });
  }

  /**
   * The next Worker's slice of the kernel's stack array, or undefined for the
   * first -- which keeps the stack the module was linked with, and is the one
   * that reports where the array is.
   */
  /**
   * Hand a Worker the closure's libraries, one message each.
   *
   * Sent after the start message rather than inside it, and never awaited:
   * the process is already loading, and a library that arrives late costs a
   * few dispatch misses rather than a boot. Failure of one is reported and
   * does not stop the others -- a closure short one library still saves
   * eighteen nineteenths of the translation.
   */
  /// Send what a Worker needs to *register* the closure: metadata only.
  ///
  /// The compiled modules used to go too, one message each, and the Worker
  /// waited for all of them before it ran anything. Thirty-three of them is
  /// 549ms between a parent finishing its fork image and the child starting
  /// to adopt -- and a process enters four. Registration wants only the
  /// `gex.meta` section, which is 516 bytes for the dynamic linker and 4 KB
  /// for libc; the module itself is wanted by the branch that lands in it,
  /// and `wantLibrary` fetches it then.
  #streamClosure(w, closure) {
    const metas = (closure ?? []).map((e) => (e instanceof Uint8Array ? e : e.meta));
    // And which program each library is the image of, where it is one. Every
    // root of a closure is an image and they all claim `PIE_BASE`, so this is
    // how a Worker tells `ls`'s pages from `bash`'s.
    const programs = (closure ?? []).map((e) => (e instanceof Uint8Array ? '' : (e.program ?? '')));
    try {
      w.postMessage({ kind: 'closureMeta', metas, programs });
    } catch (e) {
      this.#onOutput(2, `gex: the closure metadata did not reach a worker: ${e.message}\n`, 0);
    }
  }

  /// Hand one compiled closure library to the Worker that asked for it,
  /// fetching and compiling it now if this is the first time anybody has.
  ///
  /// A closure entry may arrive with only its metadata and a `url`: the page
  /// registers sixty-two libraries from a few hundred kilobytes and leaves
  /// 239 MB of wasm on the server until a branch lands in one. The compile
  /// is kept, because the next process to ask wants the same module and a
  /// `WebAssembly.Module` clones by reference.
  async #sendLibrary(w, i) {
    const entry = (this.#extra?.closure ?? [])[i];
    if (!entry) return;
    try {
      if (!(entry instanceof Uint8Array) && !entry.mod && entry.url) {
        entry.fetching ??= (async () => {
          const t0 = performance.now();
          const res = await fetch(entry.url);
          if (!res.ok) throw new Error(`${entry.url}: ${res.status} ${res.statusText}`);
          const bytes = new Uint8Array(await res.arrayBuffer());
          const mod = await WebAssembly.compile(bytes);
          console.debug(
            `[gex] closure library ${i}: ${(bytes.length / 1048576).toFixed(1)} MiB `
            + `fetched and compiled in ${Math.round(performance.now() - t0)}ms`,
          );
          return mod;
        })();
        entry.mod = await entry.fetching;
      }
      w.postMessage({
        kind: 'closureModule',
        i,
        mod: entry instanceof Uint8Array ? entry : entry.mod,
      });
    } catch (e) {
      // Loud, and not fatal: a library that never arrives costs the guest
      // the translation it would have saved, and nothing else.
      console.error(`[gex] closure library ${i} did not reach a worker:`, e);
      this.#onOutput(2, `gex: closure library ${i} did not reach a worker: ${e.message}\n`, 0);
    }
  }

  #nextKernelStack(key) {
    if (!this.#stacks) return undefined;
    const reuse = this.#stacksFree.pop();
    if (reuse !== undefined) {
      this.#stackOf.set(key, reuse);
      return this.#stacks.area + reuse * this.#stacks.size;
    }
    // Slice 0 belongs to nobody: the first Worker is already using the
    // linked-in stack, and leaving a gap costs half a megabyte of address
    // space in a memory that has a gigabyte of it.
    const n = ++this.#stacksTaken;
    this.#stackOf.set(key, n);
    if (n >= this.#stacks.count) {
      throw new Error(
        `the global kernel has ${this.#stacks.count} stacks and this is Worker ${n + 1}. ` +
          'Raise STACK_COUNT in shims/kernel.',
      );
    }
    return this.#stacks.area + n * this.#stacks.size;
  }

  /** A Worker has ended: stop it, and put its kernel stack back. */
  #release(key, w) {
    this.#workers.delete(key);
    const slice = this.#stackOf.get(key);
    if (slice !== undefined) {
      this.#stackOf.delete(key);
      this.#stacksFree.push(slice);
    }
    w.terminate();
  }

  #onMessage(pid, w, m, resolve, reject) {
    switch (m?.kind) {
      case 'ready': {
        // How long the Worker took to exist and load its bundle, before it
        // has been given anything to do.
        if (this.#extra?.debugExit && this.#asked.has(pid)) {
          const t = performance.now() - this.#asked.get(pid);
          this.#onOutput(2, `[gex] pid ${pid}: worker ready after ${Math.round(t)}ms\n`, pid);
        }
        const msg = this.#pending.get(pid);
        this.#pending.delete(pid);
        if (msg) {
          // Awaited, because compiling the shim cannot be synchronous in a
          // browser. The Worker is idle until this arrives either way.
          this.#shimModule().then((shimMod) => {
          // The closure goes after, one library per message.
          //
          // All of them at once does not cross: eighteen compiled modules is
          // 694 MB of `WebAssembly.Module` and Chromium answers
          // `DataCloneError: Data cannot be cloned, out of memory`, which
          // arrives as a rejected promise in the page and leaves the Worker
          // having never woken. A 77 MB closure crossed and a shell booted on
          // it, so it is size and not the idea.
          //
          // The guest runs while they arrive. That is safe because the miss
          // path already forgives a page whose module has not landed: it asks
          // once, nobody can fill it, and it asks again when the next
          // deferred module shows up.
          const { closure, ...extra } = this.#extra;
          w.postMessage({
            ...extra,
            // How many to wait for. The Worker installs them before it runs
            // the guest, so it has to know when it has them all.
            closureCount: (closure ?? []).length,
            ...msg,
            kernelStack: this.#nextKernelStack(pid),
            // Compiled once, here, rather than from bytes in every Worker.
            // The shim is 8.7 MB and a run of `apt-get update` makes ten
            // processes; compiling it ten times was most of that run's CPU,
            // spread over V8's background threads where a profile of the
            // main thread could not see it. A `WebAssembly.Module` is
            // structured-cloneable and the engine shares the code behind it.
            shim: shimMod,
            guest: this.#guest,
            kernel: this.#kernel ?? undefined,
          });
          // Timed separately, because the gap between a parent finishing its
          // fork image and a child starting to adopt is 549ms and none of it
          // is accounted for anywhere. The candidates are this `postMessage`
          // -- which carries the 15.4 MiB image copy -- and the thirty-three
          // compiled modules below, one message each, of which a process
          // enters four.
          const tSent = performance.now();
          this.#streamClosure(w, closure);
          if (this.#extra?.debugExit) {
            this.#onOutput(
              2,
              `[gex] pid ${pid}: sent ${(closure ?? []).length} closure modules in `
                + `${Math.round(performance.now() - tSent)}ms\n`,
              pid,
            );
          }
          },
          );
        }
        break;
      }
      case 'wantLibrary':
        this.#sendLibrary(w, m.i);
        break;
      case 'kernelStacks':
        this.#stacks = m;
        break;
      case 'output':
        this.#onOutput(m.fd, m.text, pid, m);
        break;
      case 'fork':
        this.#forked++;
        this.#onFork(m.child, pid);
        // Started and then let run. A child is a process in its own right;
        // the difference from a switch is exactly that nobody waits for it.
        this.#spawn(m.child.pid, { kind: 'adopt', child: m.child }).catch((e) => {
          this.#onOutput(2, `gex: process ${m.child.pid} failed: ${e.message}\n`, pid);
        });
        break;
      case 'thread': {
        // A thread of a process that is already running: same memory, same
        // kernel, a Worker of its own. Nothing waits for it -- a thread ends
        // when it ends, and its process is what the caller asked about.
        this.#threads++;
        const key = `${pid}:${m.thread.tid}`;
        const w2 = this.#makeWorker();
        this.#workers.set(key, w2);
        // Keyed by the thread, so that when it ends the right Worker is
        // stopped and the right kernel stack goes back on the free list.
        const deliver = (mm) => this.#onMessage(key, w2, mm, () => {}, () => {});
        w2.addEventListener?.('message', (e) => deliver(e.data));
        w2.on?.('message', deliver);
        // A thread that throws, said out loud.
        //
        // A process Worker has had an `error` listener since it was written,
        // and its rejection is reported. A thread's had none: nothing was
        // listening, so an exception in a guest thread was swallowed whole
        // and the process it belonged to simply stopped making progress --
        // which reads as a hang, and is the hardest kind of failure to find
        // because the evidence is destroyed with the Worker that had it.
        const threadFailed = (e) => {
          this.#onOutput(2, `gex: thread ${key} failed: ${e?.message ?? String(e)}\n`, pid);
        };
        w2.addEventListener?.('error', threadFailed);
        w2.on?.('error', threadFailed);
        this.#pending.set(key, { kind: 'thread', ...m.thread });
        // `ready` arrives keyed by the *process*, so the start message is
        // looked up by pid. A thread's is keyed by tid, so it is handed over
        // here instead, once.
        const start = (mm) => {
          if (mm?.kind !== 'ready') return;
          this.#pending.delete(key);
          this.#shimModule().then((shimMod) =>
          w2.postMessage({
            ...this.#extra,
            kind: 'thread',
            tid: m.thread.tid,
            memory: m.thread.memory,
            kernelStack: this.#nextKernelStack(key),
            shim: shimMod,
            guest: this.#guest,
            kernel: this.#kernel ?? undefined,
          }),
          );
        };
        w2.addEventListener?.('message', (e) => start(e.data));
        w2.on?.('message', start);
        break;
      }
      case 'stdin': {
        // Only pid 1 has a terminal behind it; a child that reads standard
        // input reads the descriptor it inherited, which is the kernel's
        // business rather than this file's.
        if (!this.#readStdin) {
          w.postMessage({ kind: 'eof' });
          break;
        }
        Promise.resolve(this.#readStdin(m.lflag)).then((bytes) => {
          if (bytes && bytes.length) w.postMessage({ kind: 'stdin', bytes });
          else w.postMessage({ kind: 'eof' });
        });
        break;
      }
      case 'exit':
        // What each process paid, to the console, in the browser.
        //
        // The page passes no `onExit`, so these numbers -- how long a process
        // spent translating, how many blocks it lifted, how much it took from
        // what other processes had already done -- were computed and thrown
        // away. Only the Node harness ever printed them, and when the two
        // hosts disagreed there was nothing to compare. The Node side prints
        // its own, hence the guard.
        if (typeof window !== 'undefined' && m.millis !== undefined) {
          // How long the process existed, against what it can account for.
          //
          // Everything else here is work the runtime did. A process that
          // lived five seconds and can name one and a half of them is
          // spending the rest somewhere nothing measures -- guest code
          // running, or waiting -- and without the total there is no way to
          // see that the two do not add up.
          const lived = this.#asked.has(pid)
            ? Math.round(performance.now() - this.#asked.get(pid))
            : 0;
          this.#asked.delete(pid);
          console.log(
            // `guest` is the time inside `gex.run`, which is the guest
            // actually executing plus every trap out of it. `lived` minus
            // `guest` is Worker creation, adoption and the wait for a start
            // message -- and the two have to be separable, because twice now
            // a cost has been attributed to the wrong side of that line.
            `[gex] pid ${pid}: lived ${lived}ms (guest ${m.guestMillis ?? -1}ms)` +
              `, ${m.millis}ms translating ${m.blocks} blocks` +
              // How many of those the closure already had. A run where this
              // is most of `blocks` is not a coverage problem in the static
              // recompiler -- it is the fill failing to reach the guest.
              (m.liftInClosure ? ` (${m.liftInClosure} on closure pages)` : '') +
              (m.byObject ? ` [${m.byObject}]` : '') +
              (m.installs ? `, ${m.installMs}ms installing ${m.installs} modules` +
            ` (${m.compileMs}ms compile, ${m.instantiateMs}ms instantiate, ${m.growMs}ms grow)` : '') +
          `, worker start ${(m.shimCompileMs ?? 0) + (m.shimInstMs ?? 0) + (m.guestInstMs ?? 0)}ms` + ` (shim ${m.shimCompileMs}c/${m.shimInstMs}i, guest ${m.guestInstMs}i)` +
              (m.loader && m.loader.calls
                ? `, ld.so ${m.loader.calls} syscalls, last at ${m.loader.doneMs}ms`
                : '') +
              (m.loader && m.loader.link && m.loader.link !== 'off'
                ? `, link ${m.loader.link}` +
                  (m.loader.cost && m.loader.cost[0]
                    ? ` (${m.loader.cost[0]} regions, ` +
                      `${Math.round(m.loader.cost[1] / 1024)} KiB reread in ${m.loader.cost[3]}ms, ` +
                      `${Math.round(m.loader.cost[2] / 1024)} KiB copied in ${m.loader.cost[4]}ms)`
                    : '')
                : '') +
              (m.fill && (m.fill.filled || m.fill.unanswered)
                ? `, fill ${m.fill.filled} answered ${m.fill.unanswered} not`
                : '') +
              (m.reuse
                ? `, reuse have=${m.reuse[0]} hit=${m.reuse[1]} miss=${m.reuse[2]}` +
                  ` noident=${m.reuse[3]} served=${m.reuse[4]} stored=${m.reuse[5]} writable=${m.reuse[9]} nocode=${m.reuse[10]}`
                : ''),
          );
        }
        // The guest's own hot spots, when `?hot=1` asked for them.
        //
        // Only `run-node.mjs` could ask before, through `GEX_HOT`, so "what
        // is the guest actually doing" was a question the browser could not
        // be put. It matters now: install and translation are 17 seconds of a
        // 57 second `apt-get update` from a closure, and everything else is
        // the guest running.
        for (const line of m.hot ?? []) {
          this.#onOutput(2, `[hot] pid ${pid}: ${line}\n`, pid);
        }
        // Before the release, because releasing terminates the Worker and a
        // handler that wanted to ask it anything else would be too late.
        this.#onExit(m, pid);
        this.#release(pid, w);
        resolve(m.status);
        break;
      case 'threw':
        this.#release(pid, w);
        // Said here as well as in the Worker, because only this side knows
        // *which process* fell over -- and because a child's promise often
        // has nobody awaiting it, so rejecting alone can be silent.
        console.error(`[gex] pid ${pid} threw: ${m.message}\n${m.stack ?? ''}`);
        reject(
          Object.assign(new Error(`pid ${pid}: ${m.message}`), { guestStack: m.stack }),
        );
        break;
      default:
        this.#onOther(m, pid, w);
        break;
    }
  }
}
