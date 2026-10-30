// The runtime's kernel-call Worker: an instance of the global kernel of its
// own (web/src/kernel-helper.js) whose exports the page calls by name.
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
// `{ call, op: 'calls', list }`, a batch run in order:
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
// Answered `{ call, value: [{ r, out: [bytes...] }] }`, one per call made, or
// `{ call, error }`.

import { helperKernel } from '../../src/kernel-helper.js';

const port = globalThis.postMessage
  ? globalThis
  : (await import('node:worker_threads')).parentPort;
const post = (m, transfer) => port.postMessage(m, transfer ?? []);
const enc = new TextEncoder();

let k = null;
let mem = null;

/** Room in the scratch memory, from `at`, for `n` bytes. */
function ensure(n) {
  const have = mem.buffer.byteLength;
  if (n > have) mem.grow(Math.ceil((n - have) / 65536));
}

/** Run `list`; every argument is laid out after the last, so what a call
 *  wrote is still there when the batch is done. */
function calls(list) {
  let top = 0;
  const place = (n) => {
    const at = top;
    top = (top + n + 15) & ~15;
    ensure(top + 16);
    return at;
  };
  const results = [];
  const transfer = [];
  for (const { f, a = [], stop } of list) {
    const fn = k[f];
    if (typeof fn !== 'function') throw new Error(`the kernel has no ${f}`);
    const args = [];
    const outs = [];
    for (const x of a) {
      if (typeof x === 'number' || typeof x === 'bigint') {
        args.push(x);
      } else if (x.in !== undefined || x.ptr !== undefined) {
        const v = x.in ?? x.ptr;
        const b = typeof v === 'string' ? enc.encode(v) : v;
        const at = place(b.length);
        new Uint8Array(mem.buffer).set(b, at);
        args.push(BigInt(at));
        if (x.in !== undefined) args.push(b.length);
      } else if (x.out !== undefined || x.outPtr !== undefined) {
        const cap = x.out ?? x.outPtr;
        const at = place(cap);
        args.push(BigInt(at));
        if (x.out !== undefined) args.push(cap);
        outs.push({ at, cap, all: x.outPtr !== undefined });
      } else if (x.ref !== undefined) {
        const r = results[x.ref].r;
        args.push(x.big ? BigInt(r) : Number(r));
      } else {
        throw new Error(`${f}: an argument the kernel Worker cannot pass: ${JSON.stringify(x)}`);
      }
    }
    const r = fn(...args);
    const out = outs.map(({ at, cap, all }) => {
      const n = all ? cap : typeof r === 'number' && r >= 0 && r <= cap ? r : cap;
      const b = new Uint8Array(mem.buffer).slice(at, at + n);
      transfer.push(b.buffer);
      return b;
    });
    results.push({ r, out });
    if (stop && (typeof r === 'bigint' ? r < 0n : r < 0)) break;
  }
  return { value: results, transfer };
}

const ops = {
  async start({ spec }) {
    ({ k, mem } = await helperKernel(spec, 0, 32));
    // Its exports' names: what this kernel build has.
    return { value: Object.keys(k).filter((n) => typeof k[n] === 'function') };
  },
  calls: ({ list }) => calls(list),
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
else port.on('message', onMessage);
