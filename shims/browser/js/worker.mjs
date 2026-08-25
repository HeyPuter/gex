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
import { Gex, Env, utf8 } from './gex.mjs';

const port = await channel();

// This Worker's runtime, once its start message builds it. Module-scope so a
// `sharedModule` message (relayed sibling-Worker compiles) can reach it.
let workerGex = null;

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
    workerGex?.addSharedModule(msg.i, msg.mod);
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
    const realError = console.error.bind(console);
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
  class WorkerEnv extends Env {
    write(fd, bytes) {
      // Copied: `bytes` is a view on the memory the guest is still running in.
      if (msg.debugExit) {
        // Straight out as well as posted: a crashing Worker takes its last few
        // messages with it, and those are the ones that say why.
        console.error(utf8(bytes.slice()));
      }
      // A page wants text; a command line wants the bytes: `gzip -c` writes a
      // deflate stream that a UTF-8 round trip would fill with U+FFFD.
      port.post(
        msg.rawOutput
          ? { kind: 'output', fd, bytes: bytes.slice(), lflag: lastLflag }
          : { kind: 'output', fd, text: utf8(bytes.slice()), lflag: lastLflag },
      );
    }
    now() {
      return BigInt(Date.now()) * 1000000n;
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
      port.post({ kind: 'thread', thread });
    }
    // A module this Worker just compiled, up to the host to relay to the sibling
    // Workers of the same process so they reuse it rather than recompiling the
    // same registry bytes. A `WebAssembly.Module` crosses `postMessage` by
    // reference -- no byte copy. See `Gex#sharedModules`.
    shareModule(i, mod) {
      port.post({ kind: 'compiledModule', i, mod });
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
  if (msg.forkSkip !== undefined) globalThis.GEX_FORK_SKIP_MIB = msg.forkSkip;
  // Lightweight adopt-phase narration, without the debugExit write-flood: fast
  // enough for a deep fork to reach where it hangs. See `adoptProcess`.
  if (msg.adoptTrace) globalThis.GEX_ADOPT_TRACE = 1;
  // `?attachtrace=1`: narrate a thread Worker's attach/adopt so a stranded
  // thread (one whose wasm instantiate never completes) is visible. Off by
  // default -- it is one line per phase per thread. See threads-worker note.
  if (msg.attachTrace) globalThis.GEX_ATTACH_TRACE = 1;
  if (msg.lazyThread) globalThis.GEX_LAZY_THREAD = 1;
  // `kernelStack` is this Worker's slice of the kernel's stack array. Absent for
  // the first Worker, which keeps the linked-in stack and reads the numbers out.
  const gex = new Gex(new WorkerEnv(), {
    kernel: msg.kernel ? { ...msg.kernel, stackTop: msg.kernelStack } : null,
    // A thread joins its process's memory; a process makes one. `shared` only
    // decides whether the memory a process builds can have threads later.
    memory: msg.memory ?? null,
    shared: !!msg.shared,
  });
  // Module-scope, so a `sharedModule` message (which arrives outside this
  // handler's run/adopt/thread path) can reach this Worker's runtime.
  workerGex = gex;
  // Any sibling modules the host already had when this Worker started.
  for (const [i, mod] of msg.sharedModules ?? []) gex.addSharedModule(i, mod);

  // The guest's GPU. The renderer is on the page, so calls cross a
  // `SharedArrayBuffer` and block on `Atomics.wait` -- they must be synchronous
  // (`virgl_submit` is called from inside a syscall). Before `load`: the vtest
  // socket is bound while the namespace is built, and Mesa's winsys does not
  // fall back when it is missing.
  if (msg.virgl) {
    const { VirglProxy } = await import('./virgl-bridge.mjs');
    gex.useVirgl(new VirglProxy(msg.virgl, () => port.post({ kind: 'virgl' })), !!msg.virglTrace);
  }
  // The GL device (gles-gears): the page's GlDevice, reached over a ring in a
  // SharedArrayBuffer. `useGl` sets the flag `gex_set_gl` reads at load, so it
  // must run before `load` -- which mounts /dev/gex-gl only when the flag is on.
  if (msg.glChannel) {
    const { GlProxy } = await import('./gl-bridge.mjs');
    gex.useGl(new GlProxy(msg.glChannel, () => port.post({ kind: 'gl' })));
  }
  // The Wayland compositor's present path: frames the shim commits go over this
  // ring to the page's canvas. `useWl` sets the flag `gex_set_wl` reads at load,
  // so it must run before `load` -- which binds `wayland-0` only when it is on.
  if (msg.wlChannel) {
    const { WlProxy } = await import('./wl-bridge.mjs');
    gex.useWl(new WlProxy(msg.wlChannel, () => port.post({ kind: 'wl' })));
  } else if (msg.wlStub) {
    // A headless compositor for node: the shim's gex-wayland runs and binds
    // `wayland-0`, so a guest connects and inits exactly as in the browser, but
    // frames are discarded (no canvas) and there is no input. Lets the
    // native-wayland deadlock be reproduced under node's tooling. present/
    // pollInput are what `wl_present` and the input pull call.
    // GEX_WL_DUMP=<path>: save the first committed frame as a PPM so a render
    // can be eyeballed. Loaded here (the handler is async) and closed over.
    const dumpPath = globalThis.process?.env?.GEX_WL_DUMP;
    // GEX_COVERAGE_OUT=<path>: dump every block PC translated in this process (the
    // reached set) as hex lines, to seed a reachability-trimmed closure build.
    // Re-dumped as the run reaches more code; the set only grows, so the last
    // write is the fullest. present is the natural moment -- the app has drawn.
    const covPath = globalThis.process?.env?.GEX_COVERAGE_OUT;
    const fs = (dumpPath || covPath) ? await import('node:fs') : null;
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
      present(memory, ptr, w, h, stride, format) {
        count++;
        if (count === 1 || count % 30 === 0) dumpCoverage();
        if (count === 1 || count % 60 === 0) console.error(`[wlstub] present #${count} ${w}x${h} stride=${stride} fmt=${format}`);
        if (fs && count === 1 && memory?.buffer) {
          try {
            const px = new Uint8Array(memory.buffer, ptr >>> 0, stride * h);
            const body = Buffer.alloc(w * h * 3);
            for (let y = 0; y < h; y++) for (let x = 0; x < w; x++) {
              const i = y * stride + x * 4, o = (y * w + x) * 3;
              body[o] = px[i + 2]; body[o + 1] = px[i + 1]; body[o + 2] = px[i]; // BGRA->RGB
            }
            fs.writeFileSync(dumpPath, `P6\n${w} ${h}\n255\n`);
            fs.appendFileSync(dumpPath, body);
            console.error(`[wlstub] wrote frame to ${dumpPath}`);
          } catch (e) { console.error(`[wlstub] dump failed: ${e.message}`); }
        }
      },
      pollInput() { return 0; },
    });
  }
  // The Node fast lane's network: real TCP sockets rather than Wisp, so the
  // paths above them -- apt's helper, the fetch, the parse -- run here in two
  // minutes instead of twenty.
  if (msg.network) {
    const { NodeNetwork } = await import('./net-node.mjs');
    gex.useNetwork(new NodeNetwork());
  }
  if (msg.wispUrl) {
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
    console.debug(`[worker] pid ${msg.child?.pid}: adopt arrived`);
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
          wantLibrary,
        },
      );
      if (!entry) throw new Error('the program could not be loaded');
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
    if (msg.hot) ex.gex_set_hot?.(1);
    if (msg.noIntrinsics) ex.gex_set_intrinsics?.(0);
    if (msg.intrinsicMask !== undefined) ex.gex_set_intrinsic_mask?.(msg.intrinsicMask);
    if (msg.trace) ex.gex_set_trace?.(1);
    if (msg.wholePage ?? msg.opts?.wholePage) Gex.wholePage = true;
    if (msg.traceMisses) ex.gex_set_trace_misses?.(1);
    if (msg.checkSp) ex.gex_set_check_sp?.(1);
    // `?claimpages=0`: disable per-process page numbering, so closure modules
    // publish dir entries at the build's slot numbers instead of demand-claiming
    // them. Diagnostic for the many-object-closure wrong-dispatch crash.
    if (msg.claimPages === 0) ex.gex_set_claim_pages?.(0);
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
    const runStarted = performance.now();
    const status = await gex.run(entry, ctx);
    const guestMillis = Math.round(performance.now() - runStarted);
    if (msg.exitTrace && status !== 0) {
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
    if (msg.kind !== 'thread') ex.gex_process_gone?.(134);
    // `close` before reporting: it drops the tick channel, a live handle that
    // would otherwise keep a Node Worker up.
    gex.close?.();
    port.post({
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
      // What the engine cost, which `millis` does not: compiling and
      // instantiating every module this process installed.
      installMs: gex.installMillis,
      // Split: compiling is per bytes and V8 can skip seen bytes, instantiating
      // is per module and paid every time.
      compileMs: gex.compileMillis,
      instantiateMs: gex.instantiateMillis,
      growMs: gex.growMillis,
      byObject: gex.blocksByObject,
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
    });
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
    port.post({
      kind: 'threw',
      message: (e?.message ?? String(e)) + where,
      stack: e?.stack ?? '',
    });
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
if (handed) handle(handed);
