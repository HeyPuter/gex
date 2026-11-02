// The single-thread build's system Worker (@gex/runtime-single): the global
// kernel and every guest process, in one realm with no SharedArrayBuffer.
// A process's threads are fibers of this Worker (single.mjs), each on its
// own JSPI stack, and every memory is a plain one that only this thread
// holds. The page's System reaches it as it reaches the default build's
// kernel-call Worker (kernel-worker.js), by message: the kernel's exports
// by name, the processes' host by the ops below.
//
// Here and not on the page's thread: a guest's lazy files and units are
// read with synchronous requests and its larger modules compiled
// synchronously, both of which a page's main thread refuses, and a guest's
// slice would stall the page's input.
//
// Messages in, `{ call, op, ... }`, answered `{ call, value }` or
// `{ call, error }`:
//   start { spec: { globals, arch, host } }  the page's switches (its GEX_*
//       globals, as `GEX_PAGE_SEARCH`), the guest's architecture, the
//       processes' host's options; answered with the kernel's export names
//   calls { groups }       kernel.js `calls`; answered with the wait slot's
//       word too (`seq`), once the page has one
//   slot { word }          the page's wait slot, by word index
//   slotWait { seq }       when the slot's word leaves `seq`, or 30 s: its word
//   spawnExec { r }        host.mjs `spawnExec`; answered when the process ends
//   network { net }        the page's network: null for none, else
//       `{ datagram }`, whether it has `datagram`
//   useKernelStacks { stacks }, stop
// And out, unasked: `{ ev: 'output', fd, text }`, `{ ev: 'message', m }`
// (a process's message the page's parts take: audio, net events),
// `{ ev: 'census', pids, threads, memory }` as processes come and go, and
// `{ ev: 'net-dial', host, port, datagram, reply }` for each connection out
// (net.js `pageRelay`): the page's network stays on the page, and the
// guest's sockets are here, beside the fibers that wait on them. And
// `{ ev: 'page-fs', req, cap, reply }` for each call on a directory the page
// serves or to a host call (page-fs.mjs `pageFsSuspending`), answered
// `[result, bytes]` on the port `reply`.

import { enableSingle, sched } from '../../../shims/browser/js/single.mjs';
import { runCallsAsync } from './kernel-calls.js';
import { Sockets, pageRelay } from '../../src/net.js';
import { setPageFsAsk } from '../../../shims/browser/js/page-fs.mjs';

enableSingle();

const post = (m, transfer) => globalThis.postMessage(m, transfer ?? []);

let k = null;
let mem = null;
let kernel = null;
let host = null;
let slotWord = -1;
/** The guest's sockets, on the page's network as it last said. */
const sockets = new Sockets(null);
const dial = (m, transfer) => post({ ev: 'net-dial', ...m }, transfer);
setPageFsAsk((req, cap) => new Promise((resolve) => {
  const { port1, port2 } = new MessageChannel();
  port1.onmessage = ({ data }) => {
    port1.close();
    resolve(data);
  };
  post({ ev: 'page-fs', req, cap, reply: port2 }, [req.buffer, port2]);
}));

const words = () => new Int32Array(kernel.memory.buffer);
let callsDone = Promise.resolve();
const promised = new Map();
/** The page's kernel call `f`, on a stack that may suspend. */
const invoke = (f, args) => {
  let fn = promised.get(f);
  if (!fn) promised.set(f, fn = WebAssembly.promising(k[f]));
  return fn(...args);
};
const seq = () => (slotWord < 0 ? undefined : Atomics.load(words(), slotWord));

function census() {
  post({ ev: 'census', pids: host?.pids ?? [], threads: host?.threads ?? 0, memory: kernel?.memory?.buffer?.byteLength ?? 0 });
}

const ops = {
  async start({ spec: { globals, arch, host: hostOpts } }) {
    // The page's switches, as the default build's Workers get them.
    Object.assign(globalThis, globals);
    await import('../../src/page-env.js');
    const q = new URLSearchParams(globalThis.GEX_PAGE_SEARCH ?? '');
    const { setGlobals } = await import('../../src/page-globals.js');
    setGlobals((name) => q.get(name), { arch });
    // A fiber runs worker.mjs's code: it registers itself (single.mjs
    // `registerWorkerMain`) as it loads.
    const [, rt] = await Promise.all([import('../../../shims/browser/js/worker.mjs'), import('../../src/runtime.js')]);
    // The shim and seed (the processes' host's) are fetched while the
    // kernel is: one wait for the network, not two in turn.
    const runtime = rt.loadRuntime();
    runtime.catch(() => {});
    [globalThis.__gexTso, kernel] = await Promise.all([rt.detectTso(), rt.loadKernel()]);
    if (!kernel) throw new Error('the kernel would not load');
    await runtime;
    const { helperKernel } = await import('../../src/kernel-helper.js');
    ({ k, mem } = await helperKernel(kernel, 0, 32, { suspending: true }));
    host = await rt.makeProcessHost({ kernel, uid: 0, ...hostOpts, network: sockets }, {
      onOutput: (fd, text) => post({ ev: 'output', fd, text: String(text) }),
      onExit: () => census(),
      // No program reads the page's console: each has its own pipes or pty.
      readStdin: () => new Promise(() => {}),
      onMessage: (m) => {
        // A process's memory or pixels stay here (the compositor is this
        // realm's); what the page's parts take goes over.
        if (m?.kind !== 'audio' && m?.kind !== 'net') return;
        try {
          post({ ev: 'message', m });
        } catch {}
      },
    });
    return { value: Object.keys(k).filter((n) => typeof k[n] === 'function') };
  },
  // A call may suspend (a page directory's answer), on a stack of its own;
  // the page's instance has one stack, so one batch at a time.
  calls({ groups }) {
    const run = async () => {
      const value = [];
      const transfer = [];
      for (const list of groups) {
        const r = await runCallsAsync(k, mem, list, invoke);
        value.push(r.value);
        transfer.push(...r.transfer);
      }
      // What the page's calls woke: a fiber parked on its wait slot looks.
      sched.checkSlots();
      return { value, transfer, seq: seq() };
    };
    const done = callsDone.then(run);
    callsDone = done.catch(() => {});
    return done;
  },
  slot({ word }) {
    slotWord = word;
    return { value: seq() };
  },
  async slotWait({ seq: at }) {
    if (slotWord >= 0) await sched.parkSlot(words, slotWord, at, 30e9);
    return { value: seq() };
  },
  spawnExec({ r }) {
    const done = host.spawnExec(r);
    census();
    return done.then(() => ({ value: null }));
  },
  network({ net }) {
    sockets.network = net ? pageRelay(dial, net) : null;
    return { value: null };
  },
  useKernelStacks({ stacks }) {
    host.useKernelStacks(stacks);
    return { value: null };
  },
  async stop() {
    await host?.stop();
    return { value: null };
  },
};

globalThis.onmessage = async ({ data: m }) => {
  if (m?.call === undefined) return;
  try {
    const { value, transfer, seq: s } = await ops[m.op](m);
    post({ call: m.call, value, seq: s }, transfer);
  } catch (e) {
    post({ call: m.call, error: { message: e?.message ?? String(e), stack: e?.stack } });
  }
};
