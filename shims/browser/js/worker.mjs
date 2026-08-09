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

port.on(async (msg) => {
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
  if (msg?.kind !== 'run' && msg?.kind !== 'adopt') return;
  const { Gex, Env } = await import(msg.gexUrl ?? './gex.mjs');

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
      port.post({
        kind: 'output',
        fd,
        text: new TextDecoder().decode(bytes.slice()),
        lflag: lastLflag,
      });
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
  }

  if (msg.debugExit) globalThis.GEX_DEBUG_EXIT = 1;
  // `kernelStack` is this Worker's slice of the global kernel's stack array.
  // Absent for the first Worker, which keeps the stack the module was linked
  // with and is the one that reads the numbers out for everybody else.
  const gex = new Gex(new WorkerEnv(), {
    kernel: msg.kernel ? { ...msg.kernel, stackTop: msg.kernelStack } : null,
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
  if (msg.wispUrl) {
    // The site's, because it is built on a Wisp client the site depends on
    // and this directory has no dependencies at all. Imported only when a
    // proxy was named, so a Node run that never asks for a network never
    // looks for it.
    const { WispNetwork } = await import('../../../web/src/net.js');
    gex.useNetwork(new WispNetwork(msg.wispUrl, (e) => port.post({ kind: 'net', event: e })));
  }
  try {
    let entry;
    if (msg.kind === 'adopt') {
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
    const recompile = msg.opts?.recompileObjects ?? msg.recompileObjects;
    if (recompile) {
      gex.shim.exports.gex_set_recompile_objects?.(recompile);
    }
    if (msg.heartbeat) {
      gex.shim.exports.gex_set_heartbeat?.(BigInt(msg.heartbeat));
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
    const status = await gex.run(entry);
    port.post({
      kind: 'exit',
      status,
      blocks: Number(gex.shim.exports.gex_jit_blocks?.() ?? 0),
      millis: Number(gex.shim.exports.gex_jit_millis?.() ?? 0),
    });
  } catch (e) {
    // Where the guest was, not only what the engine said. A crash inside the
    // shim reaches the host as `unreachable` and nothing else; the address
    // and the object it is in are the only things that name the cause, and
    // they are readable here and nowhere else.
    let where = '';
    try {
      const ex = gex.shim?.exports;
      if (ex?.gex_current_pc) {
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
    if (msg.debugExit) console.error(`gex: worker threw: ${e?.message ?? e}${where}`);
    port.post({ kind: 'threw', message: (e?.message ?? String(e)) + where });
  }
});

port.post({ kind: 'ready' });
