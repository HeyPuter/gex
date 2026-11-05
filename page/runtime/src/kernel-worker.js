// The runtime's kernel-call Worker: an instance of the global kernel of its
// own (page/src/kernel-helper.js) whose exports the page calls by name.
//
// It exists only because the page's thread cannot call the kernel itself:
// the kernel's locks may wait (memory.atomic.wait), and an atomic wait traps
// on a page's main thread. Everything else, the page does (kernel.js): it
// lays out what it asks, reads what comes back, and waits for the kernel's
// wait queues with Atomics.waitAsync on the kernel's own memory.
//
// Messages in: `{ call, op: 'start', spec }` (instantiate, on the kernel's
// linked stack, the one no process's Worker is given; answered with the
// exports' names), and
// `{ call, op: 'calls', groups }`, batches run in order, each one:
//
//   list: [{ f: 'k_fs_open', a: [args...], stop }]
//
// Each argument is a number or a BigInt, passed as it is, or one of:
//   { in: bytes | string }   its address (a BigInt) and its length
//   { ptr: bytes | string }  its address alone
//   { out: cap }             room for `cap` bytes: address and cap; the bytes
//                            come back cut to the result, when that is a
//                            count no larger than `cap`
//   { outPtr: cap }          room for `cap` bytes: address alone; all of it
//                            comes back
//   { ref: i, big }          what call `i` of this batch returned (a BigInt
//                            when `big`)
// `stop`: a negative result ends the batch there.
//
// Answered `{ call, value: [[{ r, out: [bytes...] }]] }`, a list per batch
// with one per call made, or `{ call, error }`.

import { helperKernel } from '../../src/kernel-helper.js';
import { runCalls, readsFiles } from './kernel-calls.js';
import { lazyReady } from '../../../hosts/browser/js/lazy-fetch.mjs';
import { setLazyCache } from '../../../hosts/browser/js/lazy-cache.mjs';

// Under Node, the parent's port once node:worker_threads is in (below; no
// top-level await, which a bundler's classic-script Worker cannot hold).
let port = globalThis.postMessage ? globalThis : null;
const post = (m, transfer) => port.postMessage(m, transfer ?? []);

let k = null;
let mem = null;
// The lazy files are in a rest bundle (`spec.restBundle`): its decoder,
// loaded at the first batch that reads a file, as a process's Worker loads
// its own at its start. None where the page's lazy-file owner reads them
// (`spec.lazyCache`), as for a process's.
let rest = false;

const ops = {
  async start({ spec }) {
    if (spec.lazyCache) setLazyCache(spec.lazyCache);
    rest = !!spec.restBundle;
    ({ k, mem } = await helperKernel(spec, 0, 32));
    // It lays a streamed bundle's files: it never waits for one (EAGAIN).
    k.k_fs_noblock?.();
    // Its exports' names: what this kernel build has.
    return { value: Object.keys(k).filter((n) => typeof k[n] === 'function') };
  },
  async calls({ groups }) {
    if (rest && readsFiles(groups)) {
      rest = false;
      globalThis.GEX_REST_BUNDLE = 1;
      await lazyReady();
    }
    const value = [];
    const transfer = [];
    for (const list of groups) {
      const r = runCalls(k, mem, list);
      value.push(r.value);
      transfer.push(...r.transfer);
    }
    return { value, transfer };
  },
};

const onMessage = async (m) => {
  if (m?.call === undefined) return;
  try {
    const { value, transfer } = await ops[m.op](m);
    post({ call: m.call, value }, transfer);
  } catch (e) {
    post({ call: m.call, error: { message: e?.message ?? String(e), stack: e?.stack } });
  }
};
if (port === globalThis) globalThis.onmessage = (e) => onMessage(e.data);
else {
  // A parent's messages wait on the port until it has a listener.
  import('node:worker_threads').then(({ parentPort }) => {
    port = parentPort;
    port.on('message', onMessage);
  });
}
