/**
 * Does an append to the module store give up when the engine will not grow it?
 *
 * The store is a growable SharedArrayBuffer; `grow` throws a RangeError both
 * when a sibling Worker grew it past the request (retried) and when the
 * engine is out of memory. The second used to be retried for good: the Worker
 * spun in `storeAppend`, and every Worker of its process with it. Here the
 * store's `grow` always throws: the append must return -1 (the registry then
 * keeps the bytes itself), and promptly. A grow that a sibling won must still
 * succeed.
 *
 * Usage: node check-module-store-oom.mjs   (run under `timeout`: a hang fails)
 */
import { storeAppend } from './gex.mjs';

const sab = new SharedArrayBuffer(1 << 20, { maxByteLength: 64 << 20 });
Atomics.store(new BigInt64Array(sab, 0, 1), 0, 8n);
const real = SharedArrayBuffer.prototype.grow;
let bad = 0;

// Out of memory: every grow throws.
sab.grow = () => { throw new RangeError('Array buffer allocation failed'); };
const t0 = Date.now();
const off = storeAppend(sab, new Uint8Array(4 << 20));
if (off !== -1) { console.log(`FAIL: an append the store could not grow for returned ${off}`); bad = 1; }
else console.log(`ok   an append the store could not grow for gives -1 (${Date.now() - t0} ms)`);

// A lost race: a sibling grows it further first, then this grow throws.
sab.grow = function (n) { real.call(this, Math.min(this.maxByteLength, n + (1 << 20))); throw new RangeError('lost'); };
const off2 = storeAppend(sab, new Uint8Array(2 << 20).fill(7));
if (off2 < 0 || new Uint8Array(sab, off2, 1)[0] !== 7) { console.log(`FAIL: an append that lost a grow race gave ${off2}`); bad = 1; }
else console.log('ok   an append that lost a grow race to a sibling still lands');
process.exit(bad);
