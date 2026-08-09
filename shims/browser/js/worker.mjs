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

port.on(async (msg) => {
  if (msg?.kind === 'stdin') {
    stdinWaiter?.(msg.bytes);
    stdinWaiter = null;
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
      port.post({ kind: 'output', fd, text: new TextDecoder().decode(bytes.slice()) });
    }
    now() {
      return BigInt(Date.now()) * 1000000n;
    }
    async readStdin() {
      if (stdinEof) return false;
      const bytes = await new Promise((r) => {
        stdinWaiter = r;
        port.post({ kind: 'stdin' });
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
  const gex = new Gex(new WorkerEnv(), { kernel: msg.kernel ?? null });
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
      // Statically recompile each object the guest maps once it proves hot,
      // rather than meeting it one dispatch miss at a time. This is the
      // feature the project exists for and it is off unless asked for.
      if (msg.opts?.recompileObjects) {
        gex.shim.exports.gex_set_recompile_objects?.(msg.opts.recompileObjects);
      }
    }
    if (msg.heartbeat) {
      gex.shim.exports.gex_set_heartbeat?.(BigInt(msg.heartbeat));
    }
    const status = await gex.run(entry);
    port.post({
      kind: 'exit',
      status,
      blocks: Number(gex.shim.exports.gex_jit_blocks?.() ?? 0),
      millis: Number(gex.shim.exports.gex_jit_millis?.() ?? 0),
    });
  } catch (e) {
    port.post({ kind: 'threw', message: e?.message ?? String(e) });
  }
});

port.post({ kind: 'ready' });
