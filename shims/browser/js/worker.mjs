// One guest process, in its own Worker, with its own address space.
//
// This is the first piece of replacing the snapshot/restore switcher. That
// switcher exists because the page has one wasm stack and one address space:
// a process that blocks is written out and another is written in. It was
// measured at 223 MiB per switch during the gears demo -- 4.4 GiB in eight
// seconds -- and the cost is not really copying speed. Every process's
// allocator starts at the same address, so two processes' libraries land on
// each other, which is why those regions cannot simply be left alone.
//
// A Worker per process removes the cause rather than the cost. `Gex` builds
// its own `WebAssembly.Memory`, so a Worker that holds one has a private two
// gigabytes -- which is what a kernel gives a process. Nothing collides,
// nothing is snapshotted, and a process that blocks blocks its own Worker
// rather than handing the page's only stack back.
//
// What is *not* here yet is everything processes share: the filesystem, the
// process table, pipes, sockets, signals, `wait4`. A Worker started from here
// runs alone. See task #24 for the three shapes that could carry those, and
// why only the shared parts need to cross.
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

port.on(async (msg) => {
  if (msg.kind !== 'run') return;
  const { Gex, Env } = await import(msg.gexUrl ?? './gex.mjs');

  // Everything the guest writes goes back to whoever started this. Posted
  // rather than accumulated: a run that takes minutes should say what it is
  // doing while it takes them, which is the whole lesson of the harness work.
  class WorkerEnv extends Env {
    write(fd, bytes) {
      port.post({ kind: 'output', fd, text: new TextDecoder().decode(bytes) });
    }
    now() {
      return BigInt(Date.now()) * 1000000n;
    }
  }

  const gex = new Gex(new WorkerEnv());
  try {
    const entry = await gex.load(
      msg.shim,
      msg.guest,
      msg.elf,
      msg.files ?? {},
      msg.args ?? [],
      msg.opts ?? {},
    );
    if (!entry) throw new Error('the program could not be loaded');
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
