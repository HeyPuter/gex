// One guest process, in its own Worker, with its own address space. The
// Worker's side of things; `host.mjs` is the other half.
//
// `Gex` builds its own `WebAssembly.Memory`, so a Worker has a private address
// space -- nothing collides, and a process that blocks blocks its own Worker.
// What processes share -- the process table, filesystem, pipes -- is in the
// global kernel, a second wasm module every Worker instantiates against one
// shared memory, so a syscall stays a call rather than a message back here.
//
// Two ways in:
//
//   `run`     load a program. This is pid 1.
//   `adopt`   become a child some other Worker's guest forked, from the
//             record that fork produced.
//
// The same file runs under a browser Worker and under Node's `worker_threads`,
// because Node is the fast lane -- a browser cycle is twenty minutes, Node two.

/** The one difference between a Web Worker and a Node one, hidden here. */
async function channel() {
  if (typeof self !== 'undefined' && typeof self.postMessage === 'function') {
    return {
      // `transfer` is optional: `postMessage(m, [])` is legal but
      // `postMessage(m, undefined)` is not, in either host.
      post: (m, transfer) =>
        transfer && transfer.length ? self.postMessage(m, transfer) : self.postMessage(m),
      on: (f) => {
        self.onmessage = (e) => f(e.data);
      },
    };
  }
  const { parentPort } = await import('node:worker_threads');
  return {
    post: (m, transfer) =>
      transfer && transfer.length
        ? parentPort.postMessage(m, transfer)
        : parentPort.postMessage(m),
    on: (f) => parentPort.on('message', f),
  };
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


// Static, not a run-time specifier: a bundler cannot resolve `import(msg.url)`,
// so the built Worker chunk 404'd on a bare `./gex.mjs` and rejected where
// nobody listened. This file and `gex.mjs` are siblings everywhere.
import { noteFetch } from './fetch-note.mjs';
import { Gex, Env, utf8, narrate, hostNowNs } from './gex.mjs';
import { single, registerWorkerMain, sched } from './single.mjs';
// GEX_WATCH_DIR=<page>: narrate every write to that page's directory entry.
if (typeof process !== 'undefined' && process.env?.GEX_WATCH_DIR) {
  Gex.watchDir = Number(process.env.GEX_WATCH_DIR);
}

// Under a block trace at node, keep the trace (the shim's `debug` channel,
// console.debug -> stdout) on the same stream as the syscall log and the crash
// report (stderr): two streams interleave arbitrarily and the stdout tail is
// lost when the process is killed, which hides the very blocks before a crash.
if (typeof process !== 'undefined' && (process.env?.GEX_TRACE_BLOCKS || process.env?.GEX_SYNC_CONSOLE)) {
  // Synchronous, too: a Worker's console goes through the main thread
  // asynchronously, and the megabytes a crashing process traces just before
  // it is killed never arrive -- the one stretch the trace is for.
  const fs = await import('node:fs');
  const sync = (...a) => {
    try { fs.writeSync(2, a.map((x) => (typeof x === 'string' ? x : String(x))).join(' ') + '\n'); } catch {}
  };
  console.error = sync;
  console.debug = sync;
}

// Everything below is one Worker's: its port, its process, its state. A
// function rather than module scope so the single-thread build (single.mjs)
// can run many of these in one realm, one per fiber; a real Worker runs one.
async function workerMain(port) {


// This Worker's runtime, once its start message builds it. Module-scope so a
// `sharedModule` message (relayed sibling-Worker compiles) can reach it.
let workerGex = null;
/// Whether this process's memory can go to the next fork child when it exits
/// (`Gex#recycleMemory`): not once another Worker holds it -- a thread, the
/// compositor reading its pixels, the session reaching its door.
let recyclable = true;

// Standard input, as it arrives. A guest read that finds nothing asks the
// host and waits here; `readStdin` below is what the run loop awaits.
let stdinWaiter = null;
let stdinEof = false;
// `lflag` reads the guest's termios through `gex_tty_lflag`, which takes the
// kernel -- so it may only be called from outside the shim. Calling it from the
// `write` import (inside a syscall, kernel already borrowed) took the borrow
// twice and panicked. It is read when the guest parks for input and remembered
// for the writes in between.
let lflag = () => 0;
let lastLflag = 0;
let winsize = () => {};

// Nothing in here may fail quietly: the handler is async, so a throw becomes a
// rejected promise nobody holds, and a Worker that rejects on the way in looks
// exactly like one that is busy.
port.on((msg) => {
  handle(msg).catch((e) => {
    const what = e?.stack ?? e?.message ?? String(e);
    if (typeof console !== 'undefined') console.error('gex: worker failed:', what);
    port.post({ kind: 'threw', message: what });
  });
});

/**
 * The guest's hottest back edges, as lines of `count  where`. Read here
 * because naming an address needs this thread's shim and memory.
 */
function hotBlocks(gex) {
  const e = gex.shim.exports;
  if (!e.gex_hot_blocks) return [];
  const at = e.gex_alloc(4096) >>> 0;
  const n = e.gex_hot_blocks(at, 4096) >>> 0;
  const text = utf8(new Uint8Array(gex.memory.buffer, at, n)).trim();
  if (!text) return [];
  return text.split('\n').map((line) => {
    const [pc, count] = line.split(' ');
    // An address alone is a library and an offset nobody has worked out yet.
    const nameAt = e.gex_alloc(160) >>> 0;
    const len = e.gex_object_at ? e.gex_object_at(BigInt(pc), nameAt, 160) >>> 0 : 0;
    const where = len
      ? utf8(new Uint8Array(gex.memory.buffer, nameAt, len))
      : pc;
    return `${String(count).padStart(10)}  ${where}`;
  });
}

// Closure libraries arrive in their own messages -- all of them in the start
// message will not cross a `postMessage` (Chromium `DataCloneError, out of
// memory`). Collected here and installed by `load` before the guest runs:
// installing them after it starts left libraries without slots and the shell
// never printed a prompt.
const arrived = [];
let expected = 0;
let allHere = null;
let allHereIs = null;

function noteClosureModule(entry) {
  arrived.push(entry);
  if (allHereIs && arrived.length >= expected) allHereIs();
}

/// Compiled libraries this Worker has asked the host for, by index.
const wanted = new Map();

/// Ask the host for closure library `i`, and answer when it lands. The module
/// is fetched only for the branch that lands in it; registration needs only
/// the metadata. Sending all modules up front cost 549ms per fork.
function wantLibrary(i) {
  let w = wanted.get(i);
  if (!w) {
    let settle;
    w = { p: new Promise((r) => { settle = r; }), settle };
    wanted.set(i, w);
    port.post({ kind: 'wantLibrary', i });
  }
  return w.p;
}

/** Every closure library this Worker was promised, once they are all in. */
function closureModules(n) {
  expected = n;
  if (!n || arrived.length >= n) return Promise.resolve(arrived);
  allHere = allHere ?? new Promise((r) => { allHereIs = r; });
  if (arrived.length >= expected) allHereIs();
  return allHere.then(() => arrived);
}

async function handle(msg) {
  if (msg?.kind === 'closureMeta') {
    // Registration only: which library owns which page. The compiled module
    // for each is asked for when a branch lands in it.
    (msg.metas ?? []).forEach((meta, i) =>
      noteClosureModule({ meta, mod: null, program: (msg.programs ?? [])[i] ?? '' }));
    expected = arrived.length;
    allHereIs?.();
    return;
  }
  if (msg?.kind === 'closureModule') {
    // The answer to a `wantLibrary`.
    if (typeof msg.i === 'number') {
      const w = wanted.get(msg.i);
      if (arrived[msg.i]) arrived[msg.i].mod = msg.mod;
      w?.settle(msg.mod);
      return;
    }
    noteClosureModule(msg.entry);
    return;
  }
  if (msg?.kind === 'stdin') {
    stdinWaiter?.(msg.bytes);
    stdinWaiter = null;
    return;
  }
  if (msg?.kind === 'winsize') {
    winsize(msg.rows, msg.cols);
    return;
  }
  if (msg?.kind === 'eof') {
    stdinEof = true;
    stdinWaiter?.(null);
    stdinWaiter = null;
    return;
  }
  // A sibling Worker's compiled module, relayed by the host so this Worker's
  // `#catchUp` reuses it. Arrives independently of the run/adopt/thread start,
  // and only matters once this Worker has a `Gex`; before that it is dropped and
  // the module is simply recompiled. See `Gex#sharedModules`.
  if (msg?.kind === 'sharedModule') {
    workerGex?.addSharedModule(msg.key, msg.mod);
    return;
  }
  // The wineserver's door, from the host: this Worker's door calls go to it
  // from its next one. Before this Worker has a `Gex`, the start message
  // carries it instead.
  if (msg?.kind === 'door') {
    workerGex?.setDoor(msg.door);
    return;
  }
  if (msg?.kind !== 'run' && msg?.kind !== 'adopt' && msg?.kind !== 'thread') return;

  // How many traps a forked child takes before installing its parent's
  // modules. Carried in the start message, not read off a global: the page's
  // global is a different context, so `?adopt=N` selected nothing. Set before
  // anything reads it.
  if (msg.adoptAfter !== undefined) globalThis.GEX_ADOPT_AFTER = msg.adoptAfter;
  // Mark this Worker's console errors: Chrome logs every level as INFO, so a
  // fault is indistinguishable from progress once read outside, and a Worker
  // cannot reach the page's reporter. Guarded so it is applied once.
  if (!globalThis.__gexMarkedConsole) {
    globalThis.__gexMarkedConsole = true;
    // Under Node, straight to fd 2: a Worker's console is a stream its event
    // loop drains to the parent, so a process that parks in the shim (wait
    // queues, d26f0d41: no park bound brings it back to JS) and then exits
    // lost every line it logged meanwhile -- unitcache-fork's GEX_DENSE_DEBUG
    // adopts among them.
    const fs = globalThis.process?.getBuiltinModule?.('fs');
    const realError = fs
      ? (...a) => {
          const t = a.map((x) => (typeof x === 'string' ? x : x?.stack || String(x))).join(' ') + '\n';
          try { fs.writeSync(2, t); } catch { console.error(...a); }
        }
      : console.error.bind(console);
    // One marker per *line*, not per call: Chrome's `--enable-logging=stderr`
    // writes a console message up to its first newline and drops the rest, so a
    // multi-line diagnostic (a `killed by signal` report with its thread,
    // syscall and futex rings) reached the harness as only its first line and
    // the part naming the fault was silently lost. Splitting here means every
    // line is its own INFO:CONSOLE record, marked and captured. An Error is
    // rendered through its stack so that survives too; a lone single-line call
    // is passed through untouched, objects and all.
    console.error = (...a) => {
      const text = a
        .map((x) => (typeof x === 'string' ? x : x?.stack || String(x)))
        .join(' ');
      if (text.includes('\n')) {
        for (const line of text.split('\n')) realError('[gex-console-error]', line);
      } else {
        realError('[gex-console-error]', ...a);
      }
    };
  }
  // Set before anything reads it, for the same reason as `adoptAfter`:
  // `forkModules()` runs in the Worker that builds the fork record, so the
  // page's global never reached it. See task 145.
  if (msg.forkMods !== undefined) globalThis.GEX_FORK_MODULES = msg.forkMods;

  // Everything the guest writes goes back to whoever started this, posted
  // rather than accumulated so a long run says what it is doing as it runs.
  // How many output messages this Worker has posted, and how many the host
  // says it has delivered (a shared counter it bumps per message).
  let outputSent = 0;
  const outputAck = msg.outputAck ? new Int32Array(msg.outputAck) : null;
  // Unit-cache files the page fetched ahead (`file:<rel>` in preModules).
  const preFiles = new Map((msg.preModules ?? []).filter(([k]) => k.startsWith('file:')).map(([k, v]) => [k.slice(5), v]));
  // `closed` (an ejected page's units archive): the page fetched every unit
  // file there is, so a file not among them is a miss, not a request.
  const preClosed = (msg.preModules ?? []).some(([k]) => k === 'closed');
  // A zstd decoder for a split unit's cold clusters (gex_split), made
  // before the guest runs: its loads are synchronous.
  let zstd = null;
  if (msg.cgen?.zstd && typeof XMLHttpRequest === 'function') {
    try {
      const { ZSTDDecoder } = await import(msg.cgen.zstd);
      zstd = new ZSTDDecoder();
      await zstd.init();
    } catch (err) {
      console.error(`[gex] no zstd decoder for cold clusters (${msg.cgen.zstd}): ${err?.message ?? err}`);
      zstd = null;
    }
  }
  class WorkerEnv extends Env {
    /**
     * Wait until the host has delivered everything this Worker wrote: called
     * by the shim just before its process's exit becomes visible to a
     * waiter, so a parent that `wait`s for a child and then prints cannot
     * overtake the child's last output (they cross different ports).
     * Bounded, so a host that has gone away does not hang the exit.
     */
    drainOutput() {
      if (!outputAck) return;
      const deadline = Date.now() + 10000;
      for (;;) {
        const got = Atomics.load(outputAck, 0);
        if (got >= outputSent) return;
        const left = deadline - Date.now();
        if (left <= 0) {
          console.error(`[gex] exit: the host delivered ${got} of ${outputSent} output messages in 10s; exiting anyway`);
          return;
        }
        Atomics.wait(outputAck, 0, got, Math.min(left, 100));
      }
    }
    write(fd, bytes) {
      // Copied: `bytes` is a view on the memory the guest is still running in.
      if (msg.debugExit) {
        // Straight out as well as posted: a crashing Worker takes its last few
        // messages with it, and those are the ones that say why.
        console.error(utf8(bytes.slice()));
      }
      // A page wants text; a command line wants the bytes: `gzip -c` writes a
      // deflate stream that a UTF-8 round trip would fill with U+FFFD.
      outputSent++;
      port.post(
        msg.rawOutput
          ? { kind: 'output', fd, bytes: bytes.slice(), lflag: lastLflag }
          : { kind: 'output', fd, text: utf8(bytes.slice()), lflag: lastLflag },
      );
    }
    now() {
      return hostNowNs();
    }
    async readStdin() {
      if (stdinEof) return false;
      const bytes = await new Promise((r) => {
        stdinWaiter = r;
        // Outside the shim, so the kernel is free to be asked.
        lastLflag = lflag();
        port.post({ kind: 'stdin', lflag: lastLflag });
      });
      // `true` rather than bytes: a shared terminal already put the keystrokes
      // in the kernel's input queue this process reads, so pushing them again
      // here would deliver every line twice.
      if (bytes === true) return true;
      if (!bytes) return false;
      gex.pushStdin(bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes));
      return true;
    }
    // The guest forked. Handed up: only the host makes Workers.
    async spawnProcess(child) {
      port.post({ kind: 'fork', child }, forkTransfer(child));
    }
    // A thread carries the memory itself, not a copy: a shared
    // `WebAssembly.Memory` crosses `postMessage` as itself, so the other Worker
    // is on this process's address space.
    async spawnThread(thread) {
      recyclable = false;
      port.post({ kind: 'thread', thread });
    }
    // A module this Worker just compiled, up to the host to relay to the sibling
    // Workers of the same process so they reuse it rather than recompiling the
    // same registry bytes. A `WebAssembly.Module` crosses `postMessage` by
    // reference -- no byte copy. See `Gex#sharedModules`.
    shareModule(key, mod) {
      port.post({ kind: 'compiledModule', key, mod });
    }
    // The wineserver published its door (docs/wineserver-kernel.md): up to
    // the host, which hands it to every Worker of the session.
    publishDoor(door) {
      recyclable = false;
      port.post({ kind: 'doorPublish', door });
    }
    // A function-tier unit's files by key (`gex.useCgen`): `msg.cgen.root`, a
    // directory under Node (GEX_CGEN_CACHE's layout), or `msg.cgen.url`, read
    // with a synchronous request -- allowed in a Worker, and what lets the
    // nested callback loop install one too. Null when there is none.
    loadUnit(key) {
      // The entries now; the module's bytes only if this Worker compiles it
      // (most get the Module from a sibling: see `Gex#cgenModule`). Fetching
      // every unit's bytes in every thread's Worker, only to use a shared
      // Module, cost each new guest thread seconds at start-up.
      const entries = this.loadFile(`${key}/c.entries`);
      if (!entries) return null;
      const env = this;
      let wasm;
      return {
        entries,
        get wasm() {
          wasm ??= env.loadFile(`${key}/c.wasm`);
          return wasm;
        },
      };
    }
    // `len` bytes at `off` of a unit-cache file (a split unit's
    // `cold.bin`), synchronously; null when they cannot be read.
    loadRange(rel, off, len) {
      const c = msg.cgen;
      if (!c) return null;
      try {
        if (c.root) {
          const fs = globalThis.process?.getBuiltinModule?.('node:fs');
          if (!fs) return null;
          const fd = fs.openSync(`${c.root}/${rel}`, 'r');
          try {
            const b = new Uint8Array(len);
            return fs.readSync(fd, b, 0, len, off) === len ? b : null;
          } finally {
            fs.closeSync(fd);
          }
        }
        const x = new XMLHttpRequest();
        x.open('GET', `${c.url}/${rel}`, false);
        x.responseType = 'arraybuffer';
        x.setRequestHeader('Range', `bytes=${off}-${off + len - 1}`);
        // Said to the page, as a unit's own files are: a cold cluster is
        // code the program is waiting for.
        const done = noteFetch(`unit:${rel}`, len);
        try {
          x.send();
        } finally {
          done(x.response?.byteLength ?? 0);
        }
        const got = new Uint8Array(x.response ?? new ArrayBuffer(0));
        // 206 is the range; a server without ranges sends the whole file.
        const at = x.status === 206 ? 0 : x.status === 200 ? off : -1;
        return at >= 0 && got.length >= at + len ? got.slice(at, at + len) : null;
      } catch {
        return null;
      }
    }
    // The zstd decoder made above, or null.
    get zstd() {
      return zstd;
    }
    // Any file of the unit cache by its relative path (an export
    // provider's module: `providers/<name>-<fingerprint>.wasm`), or null.
    loadFile(rel) {
      const c = msg.cgen;
      if (!c) return null;
      // Fetched by the page ahead of time (an ejected page's preload list).
      const pre = preFiles.get(rel);
      if (pre) return pre.slice(); // an unshared copy: Module and TextDecoder refuse shared views
      if (preClosed) return null;
      try {
        if (c.root) {
          const fs = globalThis.process?.getBuiltinModule?.('node:fs');
          return fs ? new Uint8Array(fs.readFileSync(`${c.root}/${rel}`)) : null;
        }
        const x = new XMLHttpRequest();
        x.open('GET', `${c.url}/${rel}`, false);
        x.responseType = 'arraybuffer';
        const done = noteFetch(`unit:${rel}`);
        try {
          x.send();
        } finally {
          done(x.response?.byteLength ?? 0);
        }
        return x.status === 200 ? new Uint8Array(x.response) : null;
      } catch {
        return null;
      }
    }
  }

  if (msg.debugExit) {
    globalThis.GEX_DEBUG_EXIT = 1;
    // Keep the engine's wasm-frame stack for a trap, before the shim rewraps it
    // into a plain Error: the wasm frames say *where* a syscall trap happened.
    globalThis.GEX_TRAP_STACK = 1;
  }
  // And on its own, because every trap that matters happens in a Worker and the
  // switch for it was only ever set in the page. `?trapstack=1` set a global
  // here that nothing in this Worker could see, so the one diagnostic that
  // names the shim function a fault was in has never reached a forked child --
  // and `?debug=1`, which does carry it, mirrors every guest write and changed
  // the fault it was being used to look at.
  if (msg.trapStack) globalThis.GEX_TRAP_STACK = 1;
  if (msg.doorStats) globalThis.GEX_DOOR_STATS = msg.doorStats;
  if (msg.forkSkip !== undefined) globalThis.GEX_FORK_SKIP_MIB = msg.forkSkip;
  // Lightweight adopt-phase narration, without the debugExit write-flood: fast
  // enough for a deep fork to reach where it hangs. See `adoptProcess`.
  if (msg.adoptTrace) globalThis.GEX_ADOPT_TRACE = 1;
  // `?attachtrace=1`: narrate a thread Worker's attach/adopt so a stranded
  // thread (one whose wasm instantiate never completes) is visible. Off by
  // default -- it is one line per phase per thread. See threads-worker note.
  if (msg.attachTrace) globalThis.GEX_ATTACH_TRACE = 1;
  if (msg.denseDebug) globalThis.GEX_DENSE_DEBUG = 1;
  // A 64-bit guest memory (docs/mem64.md step 6): the wasm64 shim's process.
  if (msg.mem64) globalThis.GEX_MEM64 = 1;
  if (msg.wasmDlls) Gex.wasmDlls = msg.wasmDlls;
  if (msg.elfUnits) Gex.elfUnits = msg.elfUnits;
  if (msg.providerSkip) Gex.providerSkip = msg.providerSkip;
  if (msg.fjitLibs) Gex.fjitLibs = msg.fjitLibs;
  if (msg.calloutTrace) globalThis.GEX_CALLOUT_TRACE = 1;
  if (msg.lazyThread) globalThis.GEX_LAZY_THREAD = 1;
  if (msg.unitCache) globalThis.GEX_UNIT_CACHE = 1;
  // `kernelStack` is this Worker's slice of the kernel's stack array. Absent for
  // the first Worker, which keeps the linked-in stack and reads the numbers out.
  const gex = new Gex(new WorkerEnv(), {
    kernel: msg.kernel ? { ...msg.kernel, stackTop: msg.kernelStack } : null,
    // A thread joins its process's memory; a process makes one. `shared` only
    // decides whether the memory a process builds can have threads later.
    // A fork child may be given an exited process's memory, zeroed
    // (`recycledMemory`, host.mjs): the same as a new one, its pages touched.
    memory: msg.memory ?? msg.recycledMemory ?? null,
    moduleStore: msg.moduleStore ?? null,
    shared: !!msg.shared,
  });
  // Module-scope, so a `sharedModule` message (which arrives outside this
  // handler's run/adopt/thread path) can reach this Worker's runtime.
  workerGex = gex;
  if (msg.door) gex.setDoor(msg.door);
  if (msg.cgen?.skip) gex.providerSkip(msg.cgen.skip);
  if (msg.cgen?.mode) gex.useCgen(msg.cgen.mode);
  if ((msg.providers ?? msg.opts?.providers) === 0) gex.noElfProviders();
  // Any sibling modules the host already had when this Worker started.
  for (const [key, mod] of msg.sharedModules ?? []) gex.addSharedModule(key, mod);
  for (const [key, mod] of msg.preModules ?? []) if (key.startsWith('cgen:') || key.startsWith('cgenfile:')) gex.addSharedModule(key, mod);
  if (globalThis.GEX_SHARE_TRACE || globalThis.process?.env?.GEX_SHARE_TRACE || msg.shareTrace) console.warn(`[share] ${msg.kind} start with ${(msg.sharedModules ?? []).length} shared modules (${(msg.sharedModules ?? []).filter(([k]) => String(k).startsWith('cgen:')).length} units, ${(msg.sharedModules ?? []).filter(([k]) => String(k).startsWith('wasm:')).length} programs)`);

  // The GL device (gles-gears): the page's GlDevice, reached over a ring in a
  // SharedArrayBuffer. `useGl` sets the flag `gex_set_gl` reads at load, so it
  // must run before `load` -- which mounts /dev/gex-gl only when the flag is on.
  if (msg.glChannel?.inRealm) {
    // The single-thread build: the compositor's Mesa is in this realm
    // (web/src/compositor-worker.js `glInRealm`), and a submit is a promise
    // the Suspending `gl_submit` waits on. A batch is run straight out of
    // this process's memory: no copy, and no batch in flight unwaited.
    // An unwaited batch is waited for all the same; its answer is what the
    // Worker build's would be, the frames shown its PRESENT replied.
    const { shownInReply } = await import('./gl-bridge.mjs');
    gex.useGl({
      submit: (memory, ring, client, unwaited) => {
        if (!globalThis.__gexGlInRealm) return -1;
        const r = globalThis.__gexGlInRealm.submit(memory, ring, client);
        if (!unwaited) return r;
        return Promise.resolve(r).then((rc) => (rc < 0 ? rc : shownInReply(new Uint8Array(memory.buffer), ring)));
      },
    });
  } else if (msg.glChannel) {
    const { GlProxy } = await import('./gl-bridge.mjs');
    gex.useGl(new GlProxy(msg.glChannel, () => {
      // An OffscreenCanvas renderer waits on the control word directly. Keep
      // thousands of synchronous query wake-ups off the page event loop.
      if (!msg.glChannel.direct) port.post({ kind: 'gl' });
    }));
  }
  // WebGPU (/dev/gex-gpu): the wire's server, wherever the page runs it,
  // over its own shared buffers. Before `load`, which mounts the device.
  if (msg.gpuChannel) {
    const { GpuProxy } = await import('./gpu-bridge.mjs');
    gex.useGpu(new GpuProxy(msg.gpuChannel));
  }
  // The Wayland compositor: this process's window into the scene arena the
  // compositor worker draws from. `useWl` sets the flag `gex_set_wl` reads at
  // load, so it must run before `load` -- which binds `wayland-0` only when it
  // is on.
  if (msg.wlChannel) {
    const { WlProxy, outputSize } = await import('./wl-bridge.mjs');
    // The token is this process's pid, which is not known yet -- the process
    // has none until it is loaded or adopted -- so it is asked for lazily, on
    // the first publish.
    const proxy = new WlProxy(msg.wlChannel, () => port.post({ kind: 'wl' }), () => gex.wlToken());
    gex.useWl(proxy);
    // The output size a toplevel is configured to, before the guest runs.
    const out = outputSize(msg.wlChannel);
    gex.wlOutput(out.w, out.h, out.scale);
    // The compositor reads this process's pixels out of this memory, so it
    // needs the memory itself. Sent once, with the token that names this
    // process's block in the arena.
    // The compositor reads this process's pixels out of this memory, so it
    // needs the memory itself. Sent when the token is known, which is after
    // load: `onLoaded` is the first moment this process has a pid.
    // The memory object, not its buffer: a shared memory that grows gives out a
    // new buffer, and the compositor has to be able to see the new length.
    gex.onWlToken = (token) => {
      recyclable = false;
      port.post({ kind: 'wl-memory', token, memory: gex.memory, kernel: gex.kernelWake()?.memory });
    };
    proxy.onToken = gex.onWlToken;
    proxy.wakeAddr = () => gex.kernelWake();
  } else if (msg.wlStub) {
    // A headless compositor for node: the shim's gex-wayland runs and binds
    // `wayland-0`, so a guest connects and inits exactly as in the browser, but
    // there is no canvas and no input. What the browser's compositor worker
    // does in WebGL, this does in software -- painter's algorithm over the
    // published scene -- which is enough to prove a client rendered.
    //
    // GEX_WL_DUMP=<path>: composite the scene to a PPM so a render can be
    // eyeballed. GEX_COVERAGE_OUT=<path>: dump every block PC translated in
    // this process (the reached set) as hex lines, to seed a
    // reachability-trimmed closure build. Re-dumped as the run reaches more
    // code; the set only grows, so the last write is the fullest.
    const dumpPath = globalThis.process?.env?.GEX_WL_DUMP;
    const covPath = globalThis.process?.env?.GEX_COVERAGE_OUT;
    const fs = (dumpPath || covPath) ? await import('node:fs') : null;
    const bridge = await import('./wl-bridge.mjs');
    const dumpCoverage = () => {
      if (!fs || !covPath) return;
      try {
        const cov = gex.coverage();
        if (!cov) return;
        let out = '';
        for (let i = 0; i < cov.length; i++) out += '0x' + cov[i].toString(16) + '\n';
        fs.writeFileSync(covPath, out);
        console.error(`[cov] wrote ${cov.length} block PCs to ${covPath}`);
      } catch (e) { console.error(`[cov] dump failed: ${e.message}`); }
    };
    let count = 0;
    gex.useWl({
      publish(memory, ptr, n) {
        count++;
        if (count === 1 || count % 30 === 0) dumpCoverage();
        const recs = [];
        const words = new Int32Array(memory.buffer, ptr >>> 0, n * bridge.REC);
        for (let i = 0; i < n; i++) {
          const r = i * bridge.REC;
          recs.push({
            surface: words[r + bridge.R_SURFACE],
            role: words[r + bridge.R_ROLE],
            x: words[r + bridge.R_REL_X],
            y: words[r + bridge.R_REL_Y],
            w: words[r + bridge.R_WIDTH],
            h: words[r + bridge.R_HEIGHT],
            addr: words[r + bridge.R_ADDR] >>> 0,
            stride: words[r + bridge.R_STRIDE],
          });
        }
        const windows = recs.filter((s) => s.role === bridge.ROLE_TOPLEVEL && s.w > 0);
        if (count === 1 || count % 60 === 0) {
          console.error(
            `[wlstub] publish #${count}: ${recs.length} surfaces, ${windows.length} windows`
            + windows.map((w) => ` ${w.w}x${w.h}`).join(''),
          );
        }
        if (!fs || !dumpPath || !windows.length) return;
        // The output is the largest window's size, and every surface is drawn
        // into it at its own offset: one file that shows the whole session.
        const W = Math.max(...windows.map((s) => s.x + s.w));
        const H = Math.max(...windows.map((s) => s.y + s.h));
        const body = Buffer.alloc(W * H * 3);
        const px = new Uint8Array(memory.buffer);
        for (const s of recs) {
          if (s.role === bridge.ROLE_CURSOR || s.w <= 0 || s.h <= 0) continue;
          for (let y = 0; y < s.h; y++) {
            const oy = s.y + y;
            if (oy < 0 || oy >= H) continue;
            for (let x = 0; x < s.w; x++) {
              const ox = s.x + x;
              if (ox < 0 || ox >= W) continue;
              const i = s.addr + y * s.stride + x * 4;
              const o = (oy * W + ox) * 3;
              body[o] = px[i + 2];
              body[o + 1] = px[i + 1];
              body[o + 2] = px[i];
            }
          }
        }
        fs.writeFileSync(dumpPath, `P6\n${W} ${H}\n255\n`);
        fs.appendFileSync(dumpPath, body);
        if (count === 1) console.error(`[wlstub] wrote ${W}x${H} scene to ${dumpPath}`);
      },
      pollInput() { return 0; },
    });
  }

  // The Node fast lane's network: real TCP sockets rather than Wisp, so the
  // paths above them -- apt's helper, the fetch, the parse -- run here in two
  // minutes instead of twenty.
  if (msg.network) {
    const { NodeNetwork } = await import('./net-node.mjs');
    const net = new NodeNetwork();
    if (single.on) net.onActivity = () => sched.wake();
    gex.useNetwork(net);
  }
  if (msg.wispNet) {
    // The single-thread build: the system's one Wisp connection, the object
    // itself (every fiber is in its realm). Its traffic wakes parked fibers.
    msg.wispNet.onActivity = () => sched.wake();
    gex.useNetwork(msg.wispNet);
  } else if (msg.netChannel) {
    // The page's Wisp connection, called across shared memory (net-bridge.mjs):
    // a wasm32 program never hands this Worker's event loop back, so a
    // WebSocket of its own here would never deliver.
    const { NetProxy } = await import('./net-bridge.mjs');
    gex.useNetwork(new NetProxy(msg.netChannel, msg.child?.pid ?? msg.pid ?? 1));
  } else if (msg.wispUrl) {
    // The site's Wisp client, imported only when a proxy was named.
    const { WispNetwork } = await import('../../../web/src/net.js');
    const net = new WispNetwork(msg.wispUrl, (e) => port.post({ kind: 'net', event: e }));
    // Opened and waited for: constructing one only remembers the address, and
    // until the WebSocket has said hello `open` answers zero -- which the shim
    // turns into `ENETUNREACH`. A proxy that cannot be reached is not a reason
    // to refuse to run; the guest gets a network that fails and the page says so.
    try {
      await net.start();
    } catch (e) {
      port.post({ kind: 'net', event: `error ${e?.message ?? e}` });
    }
    gex.useNetwork(net);
  }
  try {
    let entry;
    // A process enters on its first thread (where `run` starts); a thread has
    // registers of its own.
    let ctx;
    if (msg.kind === 'thread') {
      const _t = (s) => { if (globalThis.GEX_ATTACH_TRACE) console.error(`[thr ${msg.tid}] ${s}`); };
      _t(`adopt start pc=0x${(msg.pc??0n).toString(16)} ctx=0x${(msg.ctx??0n).toString(16)}`);
      ({ entry, ctx } = await gex.adoptThread(msg.shim, msg.guest, msg));
      _t(`adopt done entry=0x${(entry??0n).toString(16)}, entering run`);
    } else if (msg.kind === 'adopt') {
      narrate(`[worker] pid ${msg.child?.pid}: adopt arrived`);
      // With the closure, as a fresh process gets it: a child inherits its
      // parent's code through the fork record but no record that those pages
      // are a closure's, so without this an exec'd `apt-get` throws every
      // covered library away and re-translates millions of blocks.
      // The identity too: a forked child never calls `load` (where
      // `gex_set_identity` is said), so without this it runs as uid 1000 and
      // `dpkg` refuses.
      if (msg.child) {
        msg.child.uid = msg.uid ?? msg.opts?.uid;
        msg.child.gid = msg.gid ?? msg.opts?.gid;
      }
      entry = await gex.adoptProcess(msg.shim, msg.guest, msg.child, {
        ...msg,
        closure: msg.closure ?? (await closureModules(msg.closureCount ?? 0)),
        wantLibrary,
      });
      // On the forking thread's registers, which the child's kernel keeps
      // where the parent had them (`gex_adopt_child`): `run`'s default is the
      // first thread's. A fork from any other thread entered with the first
      // thread's registers -- for W3, Wine's spawn from services.exe's RPC
      // thread started the child's `__gex_thread_start` on that thread's
      // stack, read a stack word as `clone_start`'s table index and trapped
      // ("table index is out of bounds", rpcss and every service child).
      if (msg.child?.ctx) ctx = Number(msg.child.ctx);
    } else {
      entry = await gex.load(
        msg.shim,
        msg.guest,
        msg.elf,
        msg.files ?? {},
        msg.args ?? [],
        // `closure` arrives at the top level (it is in the host's `extra` so a
        // forked child gets it too); everything else about a run is in `opts`.
        {
          ...(msg.opts ?? {}),
          // Whatever arrived in its own message, waited for here.
          closure: msg.closure ?? msg.opts?.closure
            ?? (await closureModules(msg.closureCount ?? 0)),
          // Top level as well as `opts`: a child's Worker gets the host's
          // `extra`, and pid 1's `opts` never reach it.
          link: msg.link ?? msg.opts?.link,
          dense: msg.dense ?? msg.opts?.dense,
          fjit: msg.fjit ?? msg.opts?.fjit,
          fjitMax: msg.fjitMax ?? msg.opts?.fjitMax,
          fjitOnly: msg.fjitOnly ?? msg.opts?.fjitOnly,
          liftMax: msg.liftMax ?? msg.opts?.liftMax,
          inlineDispatch: msg.inlineDispatch ?? msg.opts?.inlineDispatch,
          xlateReport: msg.xlateReport ?? msg.opts?.xlateReport,
          discoverSkip: msg.discoverSkip ?? msg.opts?.discoverSkip,
          t1Fold: msg.t1Fold ?? msg.opts?.t1Fold,
          providers: msg.providers ?? msg.opts?.providers,
          xcache: msg.xcache ?? msg.opts?.xcache,
          wantLibrary,
        },
      );
      if (!entry) throw new Error('the program could not be loaded');
      // The files are in the kernel's filesystem now, copied. The views here
      // are of the page's bundle (one SharedArrayBuffer, ~460 MiB for the
      // mspaint demo), which stays resident while anything views it: let go,
      // and tell the page so it can let go of its own.
      if (msg.files) {
        msg.files = null;
        msg.elf = null;
        // A bundle the page moved here rather than shared: freed now, not
        // whenever this Worker's isolate next collects (one that runs a guest
        // in wasm may not for the whole session). Not with
        // `ArrayBuffer.prototype.transfer(0)`: that detaches V8's view, but
        // the browser's own handle on a buffer that arrived by postMessage
        // keeps the bytes until that collection (Chromium: 400 MiB stayed
        // resident either way). Moved again, into a port already closed, the
        // message is dropped and the bytes with it.
        const gone = (msg.transfer ?? []).filter((b) => b instanceof ArrayBuffer && !b.detached);
        if (gone.length && typeof MessageChannel === 'function') {
          const ch = new MessageChannel();
          ch.port2.close();
          ch.port1.postMessage(null, gone);
          ch.port1.close();
        }
        for (const b of gone) if (!b.detached) b.transfer?.(0);
        msg.transfer = null;
        port.post({ kind: 'files-loaded' });
      }
    }

    // Statically recompile each object once it proves hot, rather than meeting
    // it one dispatch miss at a time. The feature the project exists for, off
    // unless asked for. For a forked child too: its first act is `execve`, and
    // what it maps then has never been seen.
    const recompile = msg.opts?.recompileObjects ?? msg.recompileObjects;
    if (recompile) {
      gex.shim.exports.gex_set_recompile_objects?.(recompile);
    }
    // In `opts` as well: the page passes what it knows about a run through there.
    const beat = msg.heartbeat || msg.opts?.heartbeat;
    if (beat) {
      gex.shim.exports.gex_set_heartbeat?.(BigInt(beat));
    }
    if (msg.inPlace === 0) gex.shim.exports.gex_set_in_place?.(0);
    if (msg.excTrace) gex.shim.exports.gex_set_exc_trace?.(1);
    if (msg.winsize) {
      gex.shim.exports.gex_set_winsize?.(msg.winsize.rows, msg.winsize.cols);
    }
    // Where the kernel's per-Worker stacks are, so the host can hand out the
    // next slice. Only the first Worker can ask; the rest run on a slice already.
    if (!msg.kernelStack) {
      const stacks = gex.kernelStacks?.();
      if (stacks) port.post({ kind: 'kernelStacks', ...stacks });
    }
    winsize = (rows, cols) => gex.shim.exports.gex_set_winsize?.(rows, cols);
    // The line discipline is the guest's choice, read from its termios; the page
    // does the echoing so it has to be told, and it is sent with every write
    // because that is when it changes (a shell sets up then prints a prompt).
    lflag = () => gex.shim.exports.gex_tty_lflag?.() ?? 0;
    const ex = gex.shim.exports;
    // An ejected project's AOT-only run: nothing translates, in any Worker.
    if ((msg.jit ?? msg.opts?.jit) === false) ex.gex_set_jit(0);
    // An ejected project's run, its training run included (translation on).
    if (msg.aot ?? msg.opts?.aot) ex.gex_set_aot?.(1);
    if (msg.hot) ex.gex_set_hot?.(1);
    if (msg.noIntrinsics) ex.gex_set_intrinsics?.(0);
    if (msg.intrinsicMask !== undefined) ex.gex_set_intrinsic_mask?.(msg.intrinsicMask);
    if (msg.trace) ex.gex_set_trace?.(1);
    if (msg.wholePage ?? msg.opts?.wholePage) Gex.wholePage = true;
    if (msg.vforkTrace ?? msg.opts?.vforkTrace) globalThis.GEX_VFORK_TRACE = 1;
    if (msg.traceMisses) ex.gex_set_trace_misses?.(1);
    if (msg.traceBlocks) ex.gex_set_trace_blocks?.(1);
    if (msg.watchMem) ex.gex_set_watch_mem?.(BigInt(msg.watchMem));
    if (msg.checkSp) ex.gex_set_check_sp?.(1);
    // Per-process page numbering on (1, the default) or off (0). Off publishes
    // closure dir entries at the build's slot numbers, which the profile-guided
    // wayland closures need (the demand path's streaming interaction corrupts
    // their dispatch); on keeps tables small across a many-fork run like apt.
    if (msg.claimPages !== undefined) ex.gex_set_claim_pages?.(msg.claimPages);
    if (msg.copyLoop) ex.gex_set_copy_loop?.(1);
    if (msg.coherentLoads) ex.gex_set_coherent_loads?.(1);
    if (msg.spGuard) ex.gex_set_sp_guard?.(1);
    if (msg.trapPc) ex.gex_set_trap_pc?.(BigInt(msg.trapPc));
    if (msg.pregrow) ex.gex_set_pregrow?.(1);
    if (msg.wlTrace) ex.gex_set_wl_trace?.(1);
    // A forked or exec'd child never calls `load`, so set the switch here.
    const link = msg.link ?? msg.opts?.link;
    if (link) ex.gex_set_snapshot?.(Number(link));
    if (msg.debugExit) {
      // Before the guest runs an instruction: memory decides how many processes
      // a browser holds, so whether the cost is placing the image or running it.
      port.post({ kind: 'loaded', memoryMib: gex.memory.buffer.byteLength / (1 << 20) });
    }
    // Around `gex.run` alone: everything before it is loading and module
    // compilation, so a codegen A/B measured through the total is a third
    // module size and V8's compiler.
    // A thread exits through syscall 93 without marking the whole process
    // exited, so only a process Worker should label an unreachable return as
    // an abandoned process stack.
    globalThis.GEX_REPORT_ABANDONED = msg.kind !== 'thread';
    const runStarted = performance.now();
    const status = await gex.run(entry, ctx);
    const guestMillis = Math.round(performance.now() - runStarted);
    if (msg.exitTrace && status !== 0) {
      // Who sent the last signal anywhere (the global kernel's record): a
      // signal death names its sender.
      try {
        const k = gex.kernel;
        const v = BigInt.asUintN(64, BigInt(k?.k_last_kill?.() ?? 0));
        console.warn(`[gex] pid ${msg.pid ?? '?'} ended with ${status}; last signal sent: ${Number(v & 0xffn)} from pid ${Number(BigInt.asIntN(24, v >> 40n))} to ${Number(BigInt.asIntN(32, (v >> 8n) & 0xffffffffn))} (#${k?.k_last_kill_seq?.() ?? '?'})`);
      } catch {}
      ex.gex_set_trace?.(1);
      ex.gex_dump_state?.();
      ex.gex_set_trace?.(0);
    }
    // Where each pinned object landed. A pin that does not take runs code built
    // for one address at another; only knowable after `ld.so` maps the libraries.
    if (msg.opts?.pins) ex.gex_pin_report?.();
    // A crashed process still has to die properly: `exit_process` publishes the
    // status and releases descriptors, and a wasm fault abandons the stack
    // without running it -- so a crashed child left its pipe ends shut and no
    // status for its parent, deadlocking `apt-get update`. 134 is `SIGABRT`.
    //
    // Only for a process Worker, never a thread's: `gex_process_gone` sets the
    // process-wide `EXITED` flag, and letting the first thread to exit call it
    // marked the whole process gone -- siblings read `gex_exited()` true, a
    // late-starting thread was stranded `runnable`, and the main thread hung
    // joining it. A thread's own exit already woke its joiner's futex; a thread
    // that faults is handled by the Worker's error path and never reaches here.
    // `?fjit=`: what the function JIT did, and how often its lock was held.
    if (msg.kind !== 'thread') ex.gex_fjit_report?.();
    if (msg.kind !== 'thread') ex.gex_process_gone?.(134);
    // `?xcache=`: what this process added to the store, kept for the next load
    // (docs/gexcc.md §8). After the exit is published, so a parent's wait does
    // not wait for it.
    const xcache = msg.xcache ?? msg.opts?.xcache ?? msg.child?.opts?.xcache;
    const xcacheMin = msg.xcacheMin ?? msg.opts?.xcacheMin ?? msg.child?.opts?.xcacheMin;
    if (msg.kind !== 'thread' && xcache) await gex.xcacheSave?.(xcache, xcacheMin ?? 1 << 20);
    // `close` before reporting: it drops the tick channel, a live handle that
    // would otherwise keep a Node Worker up.
    gex.close?.();
    const exitMsg = {
      kind: 'exit',
      status,
      guestMillis,
      exitPc: msg.exitTrace ? Number(ex.gex_current_pc?.() ?? 0n) : undefined,
      // Whatever the runner asked to be read out of the guest before its
      // address space goes away with this thread.
      hot: msg.hot ? hotBlocks(gex) : undefined,
      fmaCalls: msg.fmaCount ? Number(ex.gex_fma_calls?.() ?? -1) : undefined,
      extracted: (msg.extract ?? []).map((p) => [p, gex.readFile(p)]),
      blocks: Number(gex.shim.exports.gex_jit_blocks?.() ?? 0),
      // Of those, ones on a page a closure library claims: work the recompiler
      // already did and this process did again.
      liftInClosure: Number(gex.shim.exports.gex_lift_in_closure?.() ?? 0),
      // Why each miss reached the translator. See `gex_lift_why`: 0 fill not
      // possible yet, 1 no closure owner, 2 owned and already asked, 3 owned/fresh.
      liftWhy: gex.shim.exports.gex_lift_why
        ? [0, 1, 2, 3].map((i) => Number(gex.shim.exports.gex_lift_why(i)))
        : undefined,
      // For each address that translated although its span was filled, whether
      // the slot it dispatches through is written now. See #136.
      liftSlots: gex.liftSlots?.(),
      millis: Number(gex.shim.exports.gex_jit_millis?.() ?? 0),
      // What the translator did: instructions lifted, wasm bytes, translations,
      // SMC page invalidations, pages retired, retire calls, -, anonymous blocks.
      // The discovery log (GEX_DISCOVER_SKIP bit 8, run-node's GEX_DL_FILE).
      dl: (() => {
        const e = gex.shim.exports;
        const n = e.gex_dl_text?.(0, 0) >>> 0;
        if (!n) return undefined;
        const p = e.gex_alloc(n) >>> 0;
        e.gex_dl_text(p, n);
        const t = new TextDecoder().decode(new Uint8Array(gex.memory.buffer, p, n).slice());
        e.gex_free?.(p, n);
        return t;
      })(),
      xlate: gex.shim.exports.gex_xlate_stat
        ? [0, 1, 2, 3, 4, 5, 6, 7, 8, 9].map((i) => Number(gex.shim.exports.gex_xlate_stat(i)))
        : undefined,
      // What the engine cost, which `millis` does not: compiling and
      // instantiating every module this process installed.
      installMs: gex.installMillis,
      // Split: compiling is per bytes and V8 can skip seen bytes, instantiating
      // is per module and paid every time.
      compileMs: gex.compileMillis,
      unitHits: gex.unitHits,
      instantiateMs: gex.instantiateMillis,
      growMs: gex.growMillis,
      byObject: gex.blocksByObject,
      cmdline: gex.cmdline,
      // How this process waited, by kind. See `Gex.waits`.
      waits: gex.waits,
      loader: gex.loader,
      // What a Worker pays before the guest runs: shim and guest module are
      // instantiated per Worker.
      shimCompileMs: gex.shimCompileMillis,
      shimInstMs: gex.shimInstantiateMillis,
      guestInstMs: gex.guestInstantiateMillis,
      installs: gex.installCount,
      fill: gex.fillStats,
      // How the shared-slot lookup went, to say why a child re-lifts its
      // parent's libraries: pages it already had, pages the identity map knew,
      // pages it did not, pages with no identity.
      reuse: gex.shim.exports.gex_reuse_stat
        ? [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10].map((i) => Number(gex.shim.exports.gex_reuse_stat(i)))
        : null,
      // How much of this process's memory the shim used: a memory per process
      // decides how many processes a browser holds at once.
      heapKib: Number(gex.shim.exports.gex_heap_used_kib?.() ?? -1),
      // What this process's memory grew to -- the question is whether a forked
      // child pays the same again.
      memoryMib: Math.round(gex.memory.buffer.byteLength / (1 << 20)),
      // Dispatch slots the page has left, in thousands. A budget nothing
      // refills: when it runs out, the next module names a table that does not
      // exist and the engine refuses it. Per process, because the question is
      // always how many more processes a run can afford.
      slotsLeftK: Number(gex.shim.exports.gex_slots_left_k?.() ?? -1),
      // Where this process's address space went: guest mappings, the runtime's
      // own heap, Worker stacks. In MiB.
      // What this process's fork copied into it, and what `?forkskip` left out.
      forkCopied: gex.forkCopiedBytes,
      forkSkipped: gex.forkSkippedBytes,
      forkZeros: gex.forkZeroBytes,
      arena: gex.shim.exports.gex_arena_spent_mib
        ? [0, 1, 2].map((i) => Number(gex.shim.exports.gex_arena_spent_mib(i)))
        : null,
    };
    // Last: nothing calls into this instance after (`releaseWorker`).
    gex.releaseWorker?.();
    // This process's memory, zeroed, for the host to give the next fork child
    // (`Gex#recycleMemory`): its pages are touched, which a fresh memory's are
    // not. After the kernel has the process gone, so no parent waits on it.
    if (recyclable && msg.kind !== 'thread' && !msg.child?.exec) {
      exitMsg.recycle = gex.recycleMemory?.() ?? undefined;
    }
    port.post(exitMsg);
  } catch (e) {
    // Where the guest was, not only what the engine said: a crash inside the
    // shim reaches the host as `unreachable`, and the guest pc and object are
    // readable here and nowhere else.
    let where = '';
    try {
      const ex = gex.shim?.exports;
      // Only if this Worker got as far as having a kernel: a failure during
      // load left no host, and `gex_current_pc` panicking on the way in
      // replaced the real error with a bare `unreachable`.
      if (ex?.gex_current_pc && ex.gex_has_host?.()) {
        const pc = ex.gex_current_pc();
        where = ` at guest pc=0x${(pc < 0n ? pc + (1n << 64n) : pc).toString(16)}`;
        if (ex.gex_object_at) {
          const at = ex.gex_alloc(160) >>> 0;
          const n = ex.gex_object_at(pc, at, 160) >>> 0;
          if (n) {
            where += ` in ${utf8(
              new Uint8Array(gex.memory.buffer, at, n),
            )}`;
          }
        }
      }
    } catch {
      // A shim too broken to ask. The engine's message is all there is.
    }
    // The global kernel has no console of its own, so a panic there unwinds to
    // a bare `unreachable`. Its message is in its own memory, readable here
    // because this Worker made the instance.
    try {
      const k = gex.kernel;
      if (k?.k_panic_len?.()) {
        const n = k.k_panic_len() >>> 0;
        const at = k.k_panic_ptr() >>> 0;
        where += ` -- the global kernel panicked: ${utf8(
          new Uint8Array(msg.kernel.memory.buffer, at, n),
        )}`;
      }
    } catch {
      // Nothing to add.
    }
    // Always, not under a flag: the evidence above was printed only under
    // `GEX_DEBUG_EXIT` and thrown away otherwise. A debug switch should add
    // detail to a failure, never be the condition for seeing one.
    console.error(`gex: worker threw: ${e?.message ?? e}${where}\n${e?.stack ?? ''}`);
    // Before the message: until the table knows this process is gone, its
    // parent's `wait4` is parked on a child that will never report. See
    // `gex_abandon` -- #69.
    try {
      // Dump guest registers first: a trap gives JavaScript no address.
      gex?.shim?.exports?.gex_dump_state?.();
      gex?.shim?.exports?.gex_abandon?.(-9);
    } catch {
      // The shim is what just fell over; there may be nothing left to call.
    }
    // A fiber's timers outlive it unless stopped (single.mjs).
    if (single.on) gex?.close?.();
    port.post({
      kind: 'threw',
      message: (e?.message ?? String(e)) + where,
      stack: e?.stack ?? '',
    });
    // The trap abandoned this instance's stack; its slot is free for the next.
    gex?.releaseWorker?.();
  }
}

// A payload handed over at construction, if the host had one. The handshake it
// replaces -- post `ready`, be sent `adopt` -- costs one turn of the starting
// thread's event loop, and under Node that thread may be a guest about to block
// (`bash -c 'echo $(echo hi)'`), so `ready` never gets delivered and the child
// never starts. `workerData` removes the dependency. `ready` is still posted
// because a browser host has no `workerData`; sending both is harmless.
const handed = await (async () => {
  if (typeof self !== 'undefined') return null;
  try {
    const { workerData } = await import('node:worker_threads');
    return workerData?.gexStart ?? null;
  } catch {
    return null;
  }
})();

port.post({ kind: 'ready' });

// The bridges every process of a page sets up before it runs (GL, Wayland,
// network), loaded now, while this Worker waits in the host's pool: imported
// only when a process arrived, they were about a quarter of a forked child's
// start (12.8, 7.3 and 5.6 ms of 104 on apt's children, timed per phase). A
// later `import` of each is then the module already evaluated.
if (typeof self !== 'undefined') {
  import('./gl-bridge.mjs').catch(() => {});
  import('./wl-bridge.mjs').catch(() => {});
  import('./net-bridge.mjs').catch(() => {});
}
if (handed) handle(handed);
}

// A real Worker runs itself; the single-thread build registers the function
// and runs it once per fiber (single.mjs `FiberWorker`).
if (single.on) registerWorkerMain(workerMain);
else await workerMain(await channel());
