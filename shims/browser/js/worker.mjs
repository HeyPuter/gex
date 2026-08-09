// One guest process, in its own Worker, with its own address space.
//
// This is the switcher's replacement, from the Worker's side; `host.mjs` is
// the other half. That switcher existed because the page has one wasm stack
// and one address space: a process that blocks is written out and another is
// written in. It was measured at 223 MiB per switch during the gears demo --
// 4.4 GiB in eight seconds -- and the cost was not really copying speed.
// Every process's allocator starts at the same address, so two processes'
// libraries land on each other, which is why those regions could not simply
// be left alone.
//
// A Worker per process removes the cause rather than the cost. `Gex` builds
// its own `WebAssembly.Memory`, so a Worker that holds one has a private two
// gigabytes -- which is what a kernel gives a process. Nothing collides,
// nothing is snapshotted, and a process that blocks blocks its own Worker
// rather than handing the page's only stack back.
//
// What processes *do* share -- the process table, the filesystem, the pipes
// -- is in the global kernel, a second wasm module every Worker instantiates
// against one shared memory. So a syscall stays a call: none of it comes back
// through here as a message, which would be slower than the switcher was.
//
// Two ways in:
//
//   `run`     load a program. This is pid 1.
//   `adopt`   become a child some other Worker's guest forked, from the
//             record that fork produced.
//
// The same file runs under a browser Worker and under Node's
// `worker_threads`, because the fast lane for anything in this project is
// Node -- a browser cycle is twenty minutes and a Node cycle is two.

/** The one difference between a Web Worker and a Node one, hidden here. */
async function channel() {
  if (typeof self !== 'undefined' && typeof self.postMessage === 'function') {
    return {
      post: (m) => self.postMessage(m),
      on: (f) => {
        self.onmessage = (e) => f(e.data);
      },
    };
  }
  const { parentPort } = await import('node:worker_threads');
  return {
    post: (m) => parentPort.postMessage(m),
    on: (f) => parentPort.on('message', f),
  };
}

// Statically, not by a name computed at run time.
//
// It used to be `import(msg.gexUrl ?? './gex.mjs')`, and a bundler cannot
// resolve a specifier it only learns at run time -- so the built Worker chunk
// carried a bare `./gex.mjs` that resolved to nothing, 404'd, and rejected
// inside the message handler below where nobody was listening. Every demo on
// the site unpacked its rootfs and then sat there. The paths all agreed
// anyway: this file and `gex.mjs` are siblings under Node, in the flat
// directory the graphics harness serves, and in the bundle.
import { Gex, Env } from './gex.mjs';

const port = await channel();

// Standard input, as it arrives. A guest read that finds nothing asks the
// host and waits here; `readStdin` below is what the run loop awaits.
let stdinWaiter = null;
let stdinEof = false;
// Set once the guest is loaded: how the page is told about the terminal.
//
// `lflag` may only be called from outside the shim. It reads the guest's
// termios through `gex_tty_lflag`, which takes the kernel -- and the one
// place it is tempting to call it from, the `write` import, is *inside* a
// syscall with the kernel already taken. Asking there took the borrow twice,
// which panics, and the panic handler writes to fd 2, which is the same
// import again: a run that ended in a bare `unreachable` with nothing
// printed and every later call into the shim trapping on the leaked borrow.
//
// So it is read where the run loop has given the stack back -- when the guest
// parks for input -- and remembered for the writes in between.
let lflag = () => 0;
let lastLflag = 0;
let winsize = () => {};

// Nothing in here may fail quietly.
//
// The handler is asynchronous, so anything it throws becomes a rejected
// promise nobody holds -- and a Worker that rejects on the way in looks
// exactly like a Worker that is busy. That is how a Worker chunk importing a
// module that did not exist went unnoticed: every demo on the site unpacked
// its rootfs and then sat there, with no error anywhere.
port.on((msg) => {
  handle(msg).catch((e) => {
    const what = e?.stack ?? e?.message ?? String(e);
    if (typeof console !== 'undefined') console.error('gex: worker failed:', what);
    port.post({ kind: 'threw', message: what });
  });
});

/**
 * The guest's hottest back edges, as lines of `count  where`.
 *
 * Read here rather than by whoever started this Worker, because naming an
 * address needs the shim that has the object table and the memory the answer
 * is written into -- both of which belong to this thread.
 */
function hotBlocks(gex) {
  const e = gex.shim.exports;
  if (!e.gex_hot_blocks) return [];
  const at = e.gex_alloc(4096) >>> 0;
  const n = e.gex_hot_blocks(at, 4096) >>> 0;
  const text = new TextDecoder().decode(new Uint8Array(gex.memory.buffer, at, n)).trim();
  if (!text) return [];
  return text.split('\n').map((line) => {
    const [pc, count] = line.split(' ');
    // An address alone is a library and an offset nobody has worked out yet.
    const nameAt = e.gex_alloc(160) >>> 0;
    const len = e.gex_object_at ? e.gex_object_at(BigInt(pc), nameAt, 160) >>> 0 : 0;
    const where = len
      ? new TextDecoder().decode(new Uint8Array(gex.memory.buffer, nameAt, len))
      : pc;
    return `${String(count).padStart(10)}  ${where}`;
  });
}

async function handle(msg) {
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
  if (msg?.kind !== 'run' && msg?.kind !== 'adopt' && msg?.kind !== 'thread') return;

  // Everything the guest writes goes back to whoever started this. Posted
  // rather than accumulated: a run that takes minutes should say what it is
  // doing while it takes them, which is the whole lesson of the harness work.
  class WorkerEnv extends Env {
    write(fd, bytes) {
      // Copied: `bytes` is a view on the guest's memory, and posting a view
      // of a memory the guest is still running in is posting whatever it
      // writes next.
      if (msg.debugExit) {
        // Straight out as well as posted. A Worker that crashes takes its
        // last few messages with it, and the last few are exactly the ones
        // that say why -- the shim's panic goes to fd 2 and then it traps.
        console.error(new TextDecoder().decode(bytes.slice()));
      }
      // A page wants text, because it has a terminal to put it in. A command
      // line wants the bytes: `gzip -c` writes a deflate stream to standard
      // output, and a decode-and-re-encode round trip through UTF-8 turns
      // every byte that is not a character into U+FFFD.
      port.post(
        msg.rawOutput
          ? { kind: 'output', fd, bytes: bytes.slice(), lflag: lastLflag }
          : { kind: 'output', fd, text: new TextDecoder().decode(bytes.slice()), lflag: lastLflag },
      );
    }
    now() {
      return BigInt(Date.now()) * 1000000n;
    }
    async readStdin() {
      if (stdinEof) return false;
      const bytes = await new Promise((r) => {
        stdinWaiter = r;
        // Outside the shim: the run loop gave the stack back before calling
        // this, so the kernel is free to be asked.
        lastLflag = lflag();
        port.post({ kind: 'stdin', lflag: lastLflag });
      });
      if (!bytes) return false;
      gex.pushStdin(bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes));
      return true;
    }
    // The guest forked. Handed up rather than done here: only the host knows
    // how to make a Worker, and a Worker that made its own children would
    // have to outlive them.
    async spawnProcess(child) {
      port.post({ kind: 'fork', child });
    }
    // And a thread, which carries the memory rather than a copy of it. A
    // shared `WebAssembly.Memory` crosses a `postMessage` as itself, so the
    // Worker on the other side is on this process's address space and not on
    // a picture of it.
    async spawnThread(thread) {
      port.post({ kind: 'thread', thread });
    }
  }

  if (msg.debugExit) globalThis.GEX_DEBUG_EXIT = 1;
  // `kernelStack` is this Worker's slice of the global kernel's stack array.
  // Absent for the first Worker, which keeps the stack the module was linked
  // with and is the one that reads the numbers out for everybody else.
  const gex = new Gex(new WorkerEnv(), {
    kernel: msg.kernel ? { ...msg.kernel, stackTop: msg.kernelStack } : null,
    // A thread joins the memory its process already has; a process makes one.
    // `shared` only matters for the second case, and only decides whether the
    // memory this Worker builds can have threads of its own later.
    memory: msg.memory ?? null,
    shared: !!msg.shared,
  });

  // The guest's GPU. The renderer is where the canvas is -- the page -- and
  // this is a Worker, so the calls cross a `SharedArrayBuffer` and block on
  // `Atomics.wait`. They have to be synchronous: `virgl_submit` is called
  // from inside a syscall, with the guest's thread on the wasm stack.
  //
  // Before `load`, for a sharp reason: the vtest socket is bound while the
  // guest's namespace is built, and Mesa's winsys does not fall back when it
  // is missing -- it writes its handshake into the dead descriptor and blocks
  // on a reply that cannot come.
  if (msg.virgl) {
    const { VirglProxy } = await import('./virgl-bridge.mjs');
    gex.useVirgl(new VirglProxy(msg.virgl, () => port.post({ kind: 'virgl' })), !!msg.virglTrace);
  }
  // A network of this process's own. Each Worker opens its own connection to
  // the proxy rather than relaying through the page: a socket is per process
  // on a real machine too, and a relay would put the page in the path of
  // every byte a download takes.
  // The Node fast lane's network: real TCP sockets rather than Wisp. Same
  // seven calls, so everything above them -- apt's helper, the fetch, the
  // parse -- is exercised here in two minutes instead of twenty.
  if (msg.network) {
    const { NodeNetwork } = await import('./net-node.mjs');
    gex.useNetwork(new NodeNetwork());
  }
  if (msg.wispUrl) {
    // The site's, because it is built on a Wisp client the site depends on
    // and this directory has no dependencies at all. Imported only when a
    // proxy was named, so a Node run that never asks for a network never
    // looks for it.
    const { WispNetwork } = await import('../../../web/src/net.js');
    const net = new WispNetwork(msg.wispUrl, (e) => port.post({ kind: 'net', event: e }));
    // Opened here, and waited for.
    //
    // Constructing one only remembers the address; until the WebSocket has
    // said hello there is no connection, and `open` answers zero. Zero is
    // what the shim turns into `ENETUNREACH`, so every guest in every Worker
    // had a network that could not reach anything:
    //
    //   Cannot initiate the connection to deb.debian.org:80 (240.0.0.1).
    //     - connect (101: Network is unreachable)
    //
    // The page opens its own and always has, which is why the proxy showed a
    // connection per run and never a single stream: the connection was the
    // page's, and nothing that wanted to fetch anything was on it.
    //
    // A proxy that cannot be reached is not a reason to refuse to run. The
    // guest gets a network that fails, which is what it would get from a
    // machine with no route, and the page says so.
    try {
      await net.start();
    } catch (e) {
      port.post({ kind: 'net', event: `error ${e?.message ?? e}` });
    }
    gex.useNetwork(net);
  }
  try {
    let entry;
    // Which registers to enter with. A process enters on its first thread,
    // which is where `run` starts by default; a thread has one of its own.
    let ctx;
    if (msg.kind === 'thread') {
      ({ entry, ctx } = await gex.adoptThread(msg.shim, msg.guest, msg));
    } else if (msg.kind === 'adopt') {
      entry = await gex.adoptProcess(msg.shim, msg.guest, msg.child);
    } else {
      entry = await gex.load(
        msg.shim,
        msg.guest,
        msg.elf,
        msg.files ?? {},
        msg.args ?? [],
        msg.opts ?? {},
      );
      if (!entry) throw new Error('the program could not be loaded');
    }
    // Statically recompile each object the guest maps once it proves hot,
    // rather than meeting it one dispatch miss at a time. This is the feature
    // the project exists for and it is off unless asked for.
    //
    // For a forked child too: it inherits its parent's *translated* code, but
    // the first thing a shell's child does is `execve`, and what it maps then
    // has never been seen by anyone.
    // Threads of this process become Workers, when the memory allows it.
    // Told rather than guessed: the shim cannot see whether its memory is a
    // `SharedArrayBuffer`, and a `clone` staged for a host that cannot start
    // a Worker is a thread that never runs.
    if (msg.shared || msg.kind === 'thread') {
      gex.shim.exports.gex_set_thread_workers?.(1);
    }
    const recompile = msg.opts?.recompileObjects ?? msg.recompileObjects;
    if (recompile) {
      gex.shim.exports.gex_set_recompile_objects?.(recompile);
    }
    // In `opts` as well: the page passes everything it knows about a run
    // through there, and a heartbeat asked for on a demo went nowhere.
    const beat = msg.heartbeat || msg.opts?.heartbeat;
    if (beat) {
      gex.shim.exports.gex_set_heartbeat?.(BigInt(beat));
    }
    if (msg.winsize) {
      gex.shim.exports.gex_set_winsize?.(msg.winsize.rows, msg.winsize.cols);
    }
    // Where the kernel's per-Worker stacks are, so the host can give the next
    // Worker one. Only the first Worker can ask: every other is already
    // running on a slice the host handed it.
    if (!msg.kernelStack) {
      const stacks = gex.kernelStacks?.();
      if (stacks) port.post({ kind: 'kernelStacks', ...stacks });
    }
    winsize = (rows, cols) => gex.shim.exports.gex_set_winsize?.(rows, cols);
    // The line discipline is the guest's choice, read out of its termios --
    // and the page is what does the echoing and the line editing, so it has
    // to be told. Sent with every write because that is when it changes in
    // practice: a shell sets its terminal up and then prints a prompt.
    lflag = () => gex.shim.exports.gex_tty_lflag?.() ?? 0;
    // The rest of the switches. These used to be set by `run-node.mjs`, which
    // held the first guest itself; now that every process is in a Worker the
    // runner holds none of them and asks for them here instead.
    const ex = gex.shim.exports;
    if (msg.hot) ex.gex_set_hot?.(1);
    if (msg.noIntrinsics) ex.gex_set_intrinsics?.(0);
    if (msg.trace) ex.gex_set_trace?.(1);
    if (msg.traceMisses) ex.gex_set_trace_misses?.(1);
    if (msg.checkSp) ex.gex_set_check_sp?.(1);
    if (msg.debugExit) {
      // Before the guest has executed an instruction: a process's memory is
      // what decides how many of them a browser will hold, so it matters
      // whether the cost is placing the image or running it.
      port.post({ kind: 'loaded', memoryMib: gex.memory.buffer.byteLength / (1 << 20) });
    }
    // Around `gex.run` alone. Everything before it is loading, placing the
    // image and letting the engine compile a seventeen-megabyte module;
    // everything inside it is the guest. A codegen A/B measured through the
    // total is a third module size and V8's compiler.
    const runStarted = performance.now();
    const status = await gex.run(entry, ctx);
    const guestMillis = Math.round(performance.now() - runStarted);
    // Where each pinned object landed. A pin that does not take is the worst
    // outcome available -- the module was built for one address and the code
    // runs at another, so the guest branches into whatever is there -- and it
    // can only be known after the run, because `ld.so` maps the libraries.
    if (msg.opts?.pins) ex.gex_pin_report?.();
    // `close` before the reporting, not after: it drops the tick channel,
    // which is a live handle that would otherwise keep a Node Worker up.
    gex.close?.();
    port.post({
      kind: 'exit',
      status,
      guestMillis,
      // Whatever the runner asked to be read out of the guest before its
      // address space goes away with this thread.
      hot: msg.hot ? hotBlocks(gex) : undefined,
      fmaCalls: msg.fmaCount ? Number(ex.gex_fma_calls?.() ?? -1) : undefined,
      extracted: (msg.extract ?? []).map((p) => [p, gex.readFile(p)]),
      blocks: Number(gex.shim.exports.gex_jit_blocks?.() ?? 0),
      millis: Number(gex.shim.exports.gex_jit_millis?.() ?? 0),
      // How the shared-slot lookup went: pages this process already had,
      // pages the identity map knew, pages it did not, and pages with no
      // identity at all. The question is why a child re-lifts its parent's
      // libraries, and these four say which step loses them.
      reuse: gex.shim.exports.gex_reuse_stat
        ? [0, 1, 2, 3].map((i) => Number(gex.shim.exports.gex_reuse_stat(i)))
        : null,
      // How much of this process's memory the shim ever used. It decides how
      // big a memory has to be, and a memory per process decides how many
      // processes a browser will hold at once -- which is the number that
      // makes a demo work on somebody else's machine or not.
      heapKib: Number(gex.shim.exports.gex_heap_used_kib?.() ?? -1),
      // What this process's memory actually grew to. One memory per process,
      // so the interesting question is not what the *first* process costs --
      // it is whether a forked child pays the same again.
      memoryMib: Math.round(gex.memory.buffer.byteLength / (1 << 20)),
    });
  } catch (e) {
    // Where the guest was, not only what the engine said. A crash inside the
    // shim reaches the host as `unreachable` and nothing else; the address
    // and the object it is in are the only things that name the cause, and
    // they are readable here and nowhere else.
    let where = '';
    try {
      const ex = gex.shim?.exports;
      // Only if this Worker got as far as having a kernel. `gex_has_host`
      // exists for this and was not being asked: a failure *during* load left
      // no host, `gex_current_pc` panicked on the way in, and the trap that
      // panic became replaced the real error with a bare `unreachable`. The
      // diagnostic was taking down the thing it was diagnosing.
      if (ex?.gex_current_pc && ex.gex_has_host?.()) {
        const pc = ex.gex_current_pc();
        where = ` at guest pc=0x${(pc < 0n ? pc + (1n << 64n) : pc).toString(16)}`;
        if (ex.gex_object_at) {
          const at = ex.gex_alloc(160) >>> 0;
          const n = ex.gex_object_at(pc, at, 160) >>> 0;
          if (n) {
            where += ` in ${new TextDecoder().decode(
              new Uint8Array(gex.memory.buffer, at, n),
            )}`;
          }
        }
      }
    } catch {
      // A shim too broken to ask. The engine's message is all there is.
    }
    // And the global kernel, which has no console of its own: it imports two
    // memories and nothing else, so a panic in there unwinds to a bare
    // `unreachable`. Its message is in its own memory, which this Worker can
    // read because it made the instance.
    try {
      const k = gex.kernel;
      if (k?.k_panic_len?.()) {
        const n = k.k_panic_len() >>> 0;
        const at = k.k_panic_ptr() >>> 0;
        where += ` -- the global kernel panicked: ${new TextDecoder().decode(
          new Uint8Array(msg.kernel.memory.buffer, at, n),
        )}`;
      }
    } catch {
      // Nothing to add.
    }
    // The stack too, under GEX_DEBUG_EXIT. A trap out of the shim reaches
    // here as `unreachable` and nothing else; which export was on the stack
    // is the only thing that says where to look, and it is readable here.
    if (msg.debugExit) {
      console.error(`gex: worker threw: ${e?.message ?? e}${where}\n${e?.stack ?? ''}`);
    }
    port.post({ kind: 'threw', message: (e?.message ?? String(e)) + where });
  }
}

// A payload handed over at construction, if the host had one.
//
// The handshake this replaces -- post `ready`, be sent `adopt` -- costs one
// turn of the *starting* thread's event loop, and under node the starting
// thread may be a guest that is about to block. `bash -c 'echo $(echo hi)'`
// is the case: bash forks for the substitution and immediately reads the
// pipe, `k_park` calls `Atomics.wait` on node's main thread, the event loop
// stops, `ready` is never delivered, `adopt` is never sent, and the child
// that was going to write the pipe never starts.
//
// Passing the payload as `workerData` removes the dependency: the Worker has
// everything it needs the moment the thread exists, and whether its parent
// is running, blocked, or gone makes no difference to whether it starts.
//
// `ready` is still posted, because a browser host has no `workerData` and
// still uses the handshake -- and because a host that sends both is harmless:
// `start` ignores a second payload.
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
