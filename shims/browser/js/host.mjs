// The page's half of a guest system: one Worker per guest process.
//
// Each Worker has its own `WebAssembly.Memory`, so nothing collides and
// nothing is snapshotted. Shared state -- the process table, filesystem, pipes
// -- lives in the global kernel, a second wasm module every Worker
// instantiates against one shared memory, so a syscall stays a call rather
// than a message back through here. This file starts pid 1, makes a Worker per
// fork, relays I/O, and holds the run open until pid 1 exits.

/**
 * Announce a crash to an automated watcher. These failures are caught (the
 * spawn promise is rejected and handled), so no `error` event fires and a test
 * that already matched its output has stopped reading. A hook, not prose
 * scraping, which fails open. See task 146.
 */
function crashed(what) {
  try {
    globalThis.GEX_ON_CRASH?.(what);
  } catch {
    // A reporter that throws must not add a failure to the one it reports.
  }
}

/**
 * The buffers in a fork record, so `postMessage` transfers them instead of
 * copying. Views on a `SharedArrayBuffer` are skipped -- shared buffers cannot
 * be transferred, and the guest's memory is shared when threads are on; the
 * regions are `slice`d, so each owns its buffer and transferring one cannot
 * detach another.
 */
function forkTransfer(child) {
  const out = [];
  const add = (v) => {
    if (!ArrayBuffer.isView(v)) return;
    const b = v.buffer;
    if (!(b instanceof ArrayBuffer) || out.includes(b)) return;
    out.push(b);
  };
  for (const r of child?.image ?? []) add(r?.bytes);
  add(child?.vmaBlob);
  add(child?.fds);
  add(child?.pages);
  add(child?.shared);
  return out;
}

/// Where a pooled Worker's early messages are kept until somebody owns it.
const EARLY = Symbol('gex.early');

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
  // The global kernel's per-Worker stacks, as the first Worker reported them.
  // Each Worker's kernel instance has its own `__stack_pointer` global, all
  // pointing into the shared memory, so each needs its own slice or two Workers
  // in the kernel overwrite each other's locals.
  #stacks = null;
  #stacksTaken = 0;
  // Slices handed back by ended Workers, and which slice each live Worker
  // holds: a run of many short-lived threads uses as many slices as run at
  // once, not as many as it ever made.
  #stacksFree = [];
  /// The shim compiling, once, so no Worker waits on a second compile.
  #shimCompiling = null;
  /// Workers made before anybody asks, so a fork's ~170ms of Worker startup is
  /// off the path. One spare suits a shell; a pipeline wants more, and an empty
  /// pool falls back to making a Worker inline.
  #warm = [];
  #warmWanted = 2;
  #warmMade = 0;
  #warmUsed = 0;
  #stackOf = new Map();

  constructor({
    makeWorker,
    shim,
    guest,
    kernel = null,
    // Merged into every Worker's start message: everything a process needs
    // that is not the program -- GPU channel, proxy address, terminal size.
    extra = {},
    onOutput = () => {},
    onFork = () => {},
    // Every process's exit message, whole. `start` resolves with pid 1's
    // status; this hands a host the rest of what a Worker reports on exit.
    onExit = () => {},
    readStdin = null,
    // Anything a Worker posts that this file has no meaning for.
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
   * The shim, compiled once and shared to every Worker. `postMessage` clones a
   * `Uint8Array` (8.7 MB each) but clones a `WebAssembly.Module` by reference.
   */
  #shimModule() {
    if (this.#shim instanceof WebAssembly.Module) return Promise.resolve(this.#shim);
    // Async, not a preference: Chromium refuses a synchronous compile over 8 MB
    // on the main thread and the shim is 8.7. Node allows it, so the sync form
    // passed every test here and killed the page at boot.
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
   * Run `elf` as pid 1 and resolve with its exit status. Children are stopped
   * when it exits -- a page done with a program is done with its daemons.
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
    // Including the warm ones nobody claimed.
    for (const w of this.#warm) w.terminate();
    this.#warm = [];
  }

  /**
   * One process in one Worker. `msg` is `run` (load a program) or `adopt` (a
   * forked child, from its parent's fork record).
   */
  #spawn(pid, msg) {
    // When this process was asked for, to measure the gap before it runs.
    this.#asked.set(pid, performance.now());
    return new Promise((resolve, reject) => {
      const w = this.#takeWorker();
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
      // `#pending` pins the whole fork record (image bytes included) until the
      // child reports ready. A child that never starts leaves ~19 MB pinned.
      // See task 145.
      if (this.#extra?.debugExit) {
        let held = 0;
        for (const r of this.#pending.values()) {
          for (const g of r.image ?? []) held += g.bytes?.byteLength ?? 0;
        }
        console.error(
          `[pending] ${this.#pending.size} records holding ${held} bytes`,
        );
      }
      // One line per process, to `debug`: a process that never starts leaves
      // no other trace -- the fork is logged by the parent, the exit by the
      // child. #137.
      console.debug(`[host] pid ${pid}: worker taken, ${msg.kind} held for ready`);
      // And now whatever it said while it was still in the pool.
      const held = w[EARLY];
      if (held) {
        delete w[EARLY];
        w.removeEventListener?.('message', held.buf);
        w.off?.('message', held.buf);
        for (const m of held.early) this.#onMessage(pid, w, m, resolve, reject);
      }
    });
  }

  /**
   * A Worker, from the pool if one is warm. Tops up after the take, so the
   * Worker started now serves the next process, not this one.
   */
  #takeWorker() {
    const w = this.#warm.shift();
    if (w) this.#warmUsed++;
    const got = w ?? this.#makeWorker();
    this.#topUp();
    return got;
  }

  /// Start Workers up to the pool size, one per call so a burst of forks does
  /// not spend its first moments making Workers it will not reach.
  #topUp() {
    if (this.#warm.length >= this.#warmWanted) return;
    try {
      const w = this.#makeWorker();
      // Buffer whatever it says before `#spawn` attaches its listener: a
      // pooled Worker announces itself while still sitting in the array, and
      // that message would otherwise be delivered to nobody and dropped.
      const early = [];
      const buf = (e) => early.push(e && e.data !== undefined ? e.data : e);
      w.addEventListener?.('message', buf);
      w.on?.('message', buf);
      w[EARLY] = { early, buf };
      this.#warm.push(w);
      this.#warmMade++;
    } catch {
      // Cannot make another Worker: the next `#takeWorker` makes one itself.
    }
  }

  /// How the pool is doing, for the per-run line.
  get warmStats() {
    return { made: this.#warmMade, used: this.#warmUsed, idle: this.#warm.length };
  }

  /// Send a Worker the closure metadata only -- which library owns which
  /// pages. Sending all compiled modules up front cost 549ms per fork; each
  /// module is fetched later, when a branch lands in it (`#sendLibrary`).
  #streamClosure(w, closure) {
    const metas = (closure ?? []).map((e) => (e instanceof Uint8Array ? e : e.meta));
    // Which program each library images, where it is one: every closure root
    // claims `PIE_BASE`, so this tells `ls`'s pages from `bash`'s.
    const programs = (closure ?? []).map((e) => (e instanceof Uint8Array ? '' : (e.program ?? '')));
    try {
      w.postMessage({ kind: 'closureMeta', metas, programs });
    } catch (e) {
      this.#onOutput(2, `gex: the closure metadata did not reach a worker: ${e.message}\n`, 0);
    }
  }

  /// Hand one compiled closure library to the Worker that asked, fetching and
  /// compiling it now on first ask. A closure entry may arrive with only
  /// metadata and a `url`; the compile is kept and reused by reference.
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
      // Loud, not fatal: a missing library costs the guest the translation it
      // would have saved, nothing else.
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
    // Slice 0 belongs to nobody: the first Worker uses the linked-in stack.
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
        // How long the Worker took to exist and load its bundle.
        if (this.#extra?.debugExit && this.#asked.has(pid)) {
          const t = performance.now() - this.#asked.get(pid);
          this.#onOutput(2, `[gex] pid ${pid}: worker ready after ${Math.round(t)}ms\n`, pid);
        }
        const msg = this.#pending.get(pid);
        this.#pending.delete(pid);
        console.debug(`[host] pid ${pid}: ready, ${msg ? `sending ${msg.kind}` : 'NOTHING HELD'}`);
        if (msg) {
          // Awaited: compiling the shim cannot be synchronous in a browser.
          this.#shimModule().then((shimMod) => {
          // The closure goes after, one library per message: all at once does
          // not cross (Chromium `DataCloneError, out of memory`). The guest
          // runs while they arrive -- the miss path forgives a module not yet
          // landed and asks again when it shows up.
          const { closure, ...extra } = this.#extra;
          w.postMessage({
            ...extra,
            // How many to wait for. The Worker installs them before it runs
            // the guest, so it has to know when it has them all.
            closureCount: (closure ?? []).length,
            ...msg,
            kernelStack: this.#nextKernelStack(pid),
            // Compiled once here, not from bytes in every Worker.
            shim: shimMod,
            guest: this.#guest,
            kernel: this.#kernel ?? undefined,
          }, forkTransfer(msg.child));
          // Timed separately: the fork-to-adopt gap is 549ms and unaccounted --
          // this `postMessage` (the 15.4 MiB image copy) or the closure below.
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
      case 'fork': {
        const c = m.child?.forkCost;
        if (c) {
          const hop1 = Math.round(performance.timeOrigin + performance.now() - c.sentAt);
          console.log(
            `[gex] fork -> pid ${m.child.pid}: image ${(c.bytes / 1048576).toFixed(1)} MiB`
              + ` in ${c.regions} regions, built ${c.buildMs}ms,`
              + ` ${hop1}ms to reach the host`,
          );
          c.atHost = performance.now();
        }
        this.#forked++;
        this.#onFork(m.child, pid);
        // Started and let run -- nobody waits for a child.
        this.#spawn(m.child.pid, { kind: 'adopt', child: m.child }).catch((e) => {
          this.#onOutput(2, `gex: process ${m.child.pid} failed: ${e.message}\n`, pid);
          // As an event too: this is caught, so no `error` fires and a harness
          // may miss the line above. See task 146.
          crashed(`pid ${m.child.pid}: ${e.message}`);
        });
        break;
      }
      case 'thread': {
        // A thread of a running process: same memory, same kernel, its own
        // Worker. Nothing waits for it.
        this.#threads++;
        const key = `${pid}:${m.thread.tid}`;
        const w2 = this.#makeWorker();
        this.#workers.set(key, w2);
        // Keyed by the thread, so its end stops the right Worker and frees the
        // right kernel stack.
        const deliver = (mm) => this.#onMessage(key, w2, mm, () => {}, () => {});
        w2.addEventListener?.('message', (e) => deliver(e.data));
        w2.on?.('message', deliver);
        // A thread that throws, said out loud: a thread Worker had no `error`
        // listener, so an exception was swallowed and read as a hang.
        const threadFailed = (e) => {
          this.#onOutput(2, `gex: thread ${key} failed: ${e?.message ?? String(e)}\n`, pid);
          crashed(`thread ${key}: ${e?.message ?? String(e)}`);
        };
        w2.addEventListener?.('error', threadFailed);
        w2.on?.('error', threadFailed);
        this.#pending.set(key, { kind: 'thread', ...m.thread });
        // A thread's start message is keyed by tid and handed over here, once.
        const start = (mm) => {
          if (mm?.kind !== 'ready') return;
          this.#pending.delete(key);
          // Without the closure, as the fork path: `closure` entries are mostly
          // promises (a library is fetched on first branch into it), and a
          // promise is not structured-cloneable. A count instead; the Worker
          // asks for what it needs.
          const { closure: _closure, ...extra } = this.#extra ?? {};
          this.#shimModule().then((shimMod) =>
          w2.postMessage({
            ...extra,
            closureCount: (this.#extra?.closure ?? []).length,
            kind: 'thread',
            tid: m.thread.tid,
            // `ctx` and `pc` are forwarded explicitly: `adoptThread` re-derives
            // `pc`, but a dropped `ctx` became NaN -> 0, so every guest thread
            // read its registers from address 0 and died at its first
            // instruction.
            ctx: m.thread.ctx,
            pc: m.thread.pc,
            memory: m.thread.memory,
            kernelStack: this.#nextKernelStack(key),
            shim: shimMod,
            guest: this.#guest,
            kernel: this.#kernel ?? undefined,
          }),
          );
        };
        // NOT registered as a second listener: `start` is already reached
        // through `deliver` (which finds this thread in `#pending`). Registering
        // it too sent the start message twice and ran `gex.run` twice on one
        // thread. One record, one path, one start.
        void start;
        break;
      }
      case 'stdin': {
        // Only pid 1 has a terminal; a child reads the inherited descriptor,
        // which is the kernel's business, not this file's.
        if (!this.#readStdin) {
          w.postMessage({ kind: 'eof' });
          break;
        }
        Promise.resolve(this.#readStdin(m.lflag)).then((bytes) => {
          // `true` means "input is already in the shared queue you read from".
          if (bytes === true) w.postMessage({ kind: 'stdin', bytes: true });
          else if (bytes && bytes.length) w.postMessage({ kind: 'stdin', bytes });
          else w.postMessage({ kind: 'eof' });
        });
        break;
      }
      case 'exit':
        // The page passes no `onExit`, so these per-process numbers were
        // computed and thrown away. Printed here, guarded so the Node harness
        // (which prints its own) does not double up.
        if (typeof window !== 'undefined' && m.millis !== undefined) {
          // How long the process existed, against what it can account for.
          const lived = this.#asked.has(pid)
            ? Math.round(performance.now() - this.#asked.get(pid))
            : 0;
          this.#asked.delete(pid);
          console.log(
            // `guest` is the time inside `gex.run`; `lived` minus `guest` is
            // Worker creation, adoption and the wait for a start message. The
            // two must be separable -- a cost has been blamed on the wrong side.
            `[gex] pid ${pid}: lived ${lived}ms (guest ${m.guestMillis ?? -1}ms)` +
              // What the runtime still held on exit. -1 without GEX_HEAP_COUNTERS=1.
              (m.heapKib >= 0 ? `, runtime holds ${Math.round(m.heapKib / 1024)} MiB` : '') +
              // Dispatch slots the page has left. A budget nothing refills, so
              // this only falls, and how fast it falls is what says how many
              // more processes the run can translate code in.
              (m.slotsLeftK >= 0 ? `, ${m.slotsLeftK}k dispatch slots left` : '') +
              // What this process's memory grew to. One memory per process, so
              // this times the number live at once is what the tab costs.
              (m.memoryMib !== undefined ? `, memory ${m.memoryMib} MiB` : '') +
              // And what that memory went on, which is the only way to tell a
              // guest that really maps a lot from a runtime spending it.
              (m.arena ? ` (maps ${m.arena[0]}, heap ${m.arena[1]}, stacks ${m.arena[2]})` : '') +
              // The one memory every process shares: the filesystem and the
              // store of translated modules live in it, and it only grows.
              (this.#kernel?.memory
                ? `, kernel ${Math.round(this.#kernel.memory.buffer.byteLength / 1048576)} MiB`
                : '') +
              // What the fork wrote into this process. There is no
              // copy-on-write, so this is paid in full, per fork.
              (m.forkCopied
                ? `, fork copied ${Math.round(m.forkCopied / 1048576)} MiB`
                  + (m.forkZeros ? ` (${Math.round(m.forkZeros / 1048576)} MiB of it zero, left alone)` : '')
                  + (m.forkSkipped ? ` and skipped ${Math.round(m.forkSkipped / 1048576)}` : '')
                : '') +
              `, ${m.millis}ms translating ${m.blocks} blocks` +
              // Of those, ones on a page the closure already had: if most of
              // `blocks`, the fill is failing to reach the guest, not a
              // recompiler coverage gap.
              (m.liftInClosure ? ` (${m.liftInClosure} on closure pages)` : '') +
              // Why those misses translated rather than being filled.
              (m.liftWhy && m.liftWhy.some((n) => n)
                ? ` why[nofill=${m.liftWhy[0]} unowned=${m.liftWhy[1]} asked=${m.liftWhy[2]} fresh=${m.liftWhy[3]}]`
                : '') +
              (m.liftSlots ? ` slots[${m.liftSlots}]` : '') +
              (m.waits && (m.waits.park || m.waits.turn || m.waits.nap)
                ? `, waited park=${m.waits.park} turn=${m.waits.turn} nap=${m.waits.nap}`
                : '') +
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
                  + (m.fill.sysN ? `, sys ${m.fill.sysN}x ${m.fill.sysMs}ms` : '')
                  + ` (${m.fill.now ?? 0} fast, ${m.fill.nowMs ?? 0}ms[base ${m.fill.faBase ?? 0} init ${m.fill.faInit ?? 0}];`
                  + ` ${m.fill.slow ?? 0} slow, ${m.fill.slowMs ?? 0}ms;`
                  + ` ${m.fill.pub ?? 0} published, ${m.fill.pubMs ?? 0}ms,`
                  + ` grew ${m.fill.grew ?? 0} slots in ${m.fill.growMs ?? 0}ms,`
                  + ` want ${m.fill.wantMs ?? 0}ms inst ${m.fill.instMs ?? 0}ms`
                  + ` (grow ${m.fill.iGrow ?? 0} comp ${m.fill.iComp ?? 0}`
                  + ` inst ${m.fill.iInst ?? 0};`
                  + ` ${m.fill.iGrows ?? 0} grows +${m.fill.iAdded ?? 0}`
                  + ` copying ${m.fill.iCopied ?? 0}, tables ${m.fill.tlen ?? ''})`
                  + (m.fill.bigGrows ? `, big [${m.fill.bigGrows}]` : '')
                  + (m.fill.tn ? `, tn [${m.fill.tn}]` : '')
                  + ` tail ${m.fill.tailMs ?? 0}ms`
                  + (m.fill.strictGrows ? ` retry ${m.fill.strictGrows}x/${m.fill.strictSlots}` : '')
                  + `)`
                : '') +
              (m.reuse
                ? `, reuse have=${m.reuse[0]} hit=${m.reuse[1]} miss=${m.reuse[2]}` +
                  ` noident=${m.reuse[3]} served=${m.reuse[4]} stored=${m.reuse[5]} writable=${m.reuse[9]} nocode=${m.reuse[10]}`
                : ''),
          );
        }
        // The guest's own hot spots, when `?hot=1` asked for them.
        for (const line of m.hot ?? []) {
          this.#onOutput(2, `[hot] pid ${pid}: ${line}\n`, pid);
        }
        // Before the release: releasing terminates the Worker.
        this.#onExit(m, pid);
        this.#release(pid, w);
        resolve(m.status);
        break;
      case 'threw':
        this.#release(pid, w);
        // Here as well as in the Worker: only this side knows which process
        // fell over, and a child's promise often has nobody awaiting it.
        console.error(`[gex] pid ${pid} threw: ${m.message}\n${m.stack ?? ''}`);
        // As an event: the console line is INFO to a watcher and easy to miss.
        // See task 146.
        crashed(`pid ${pid} threw: ${m.message}`);
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
