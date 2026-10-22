/**
 * Do the wait queues wake exactly the waiters registered on them, at once?
 *
 * Each thread owns one wait slot (`k_ws_*`); a wait registers the slot on the
 * queue of every object it waits for (`k_wq_add`) and parks on it
 * (`k_ws_park`); whoever changes an object notifies its queue. This
 * instantiates the kernel module in several threads on shared memory and
 * parks them the way a guest's Worker does:
 *
 *   - a poll over two pipes is woken by a write to the second, within
 *     `WITHIN_MS` (1 ms on a quiet box, which is how it is timed: a quiet
 *     host, never a build box; 20 ms by default, which no bounded
 *     re-check could meet by luck against the 5 s park);
 *   - a waiter on a third pipe sleeps on through both writes (its own 600 ms
 *     timeout, 2): one object's change wakes nobody else;
 *   - a futex wake (`k_futex_wake`) ends a park on that futex's bucket queue
 *     (`k_futex_queue`);
 *   - a queue chained onto another (an epoll set onto a descriptor) is
 *     notified with it.
 *
 * Usage: node check-wait-queues.mjs [gex_kernel.wasm]
 */
import { readFile } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Worker, isMainThread, workerData, parentPort } from 'node:worker_threads';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..', '..', '..');

async function instantiate(module, memory, process_) {
  const inst = await WebAssembly.instantiate(module, {
    env: { memory, process: process_, lazy_fetch: () => 0, process64: new WebAssembly.Memory({ address: 'i64', initial: 0n, maximum: 0n, shared: true }) },
  });
  return inst.exports;
}

if (isMainThread) {
  const path = process.argv[2] || join(ROOT, 'web', 'public', 'gex', 'gex_kernel.wasm');
  const module = await WebAssembly.compile(await readFile(path));
  const memory = new WebAssembly.Memory({ initial: 2048, maximum: 65536, shared: true });
  const process_ = new WebAssembly.Memory({ initial: 1, maximum: 65535, shared: true });
  const k = await instantiate(module, memory, process_);
  const PID = 1;
  const pipe = () => {
    const id = k.k_pipe_new(PID);
    k.k_pipe_open_end(id, 0, PID);
    k.k_pipe_open_end(id, 1, PID);
    return id;
  };
  const [p1, p2, p3] = [pipe(), pipe(), pipe()];
  const ADDR = 0x1234_5670n;
  const chained = k.k_wq_new();
  const set = k.k_wq_new();
  k.k_wq_chain(chained, set);
  // Each waiter: its own slot, registered on its queues, parked for `ms`.
  // `ready` resolves once its thread is about to park.
  const ready = [];
  const park = (queues, ms) => {
    const slot = k.k_ws_alloc();
    for (const q of queues) k.k_wq_add(q, slot);
    const seq = k.k_ws_seq(slot);
    let up;
    ready.push(new Promise((r) => { up = r; }));
    return new Promise((resolve, reject) => {
      const w = new Worker(fileURLToPath(import.meta.url), {
        workerData: { module, memory, process_, slot, seq, ns: ms * 1e6 },
      });
      w.on('message', (m) => {
        if (m === 'ready') return up();
        for (const q of queues) k.k_wq_del(q, slot);
        k.k_ws_free(slot);
        resolve(m);
      });
      w.once('error', reject);
    });
  };
  const notifies0 = k.k_ws_notifies();
  const poll = park([k.k_pipe_queue(p1), k.k_pipe_queue(p2)], 5000);
  const other = park([k.k_pipe_queue(p3)], 600);
  const futex = park([k.k_futex_queue(ADDR)], 5000);
  const epoll = park([set], 5000);
  await Promise.all(ready);
  await new Promise((r) => setTimeout(r, 100));
  // A byte into the second pipe (from the process memory's first word).
  const t = performance.timeOrigin + performance.now();
  try { k.k_pipe_write(p2, 0, 1); } catch { k.k_pipe_write(p2, 0n, 1); } // PAddr: u32 or u64
  k.k_futex_wake(ADDR);
  k.k_wq_notify(chained);
  const [a, b, c, d] = await Promise.all([poll, other, futex, epoll]);
  const late = a.end - t;
  const WITHIN_MS = Number(process.env.WITHIN_MS ?? 20);
  const fail = [];
  if (a.r !== 0 || late > WITHIN_MS) fail.push(`the poll over two pipes returned ${a.r}, ${late.toFixed(2)} ms after the write to the second (want 0, within ${WITHIN_MS} ms)`);
  if (b.r !== 2 || b.ms < 500) fail.push(`a waiter on another pipe returned ${b.r} after ${b.ms.toFixed(0)} ms (want its 600 ms timeout, 2)`);
  if (c.r !== 0 || c.ms > 1000) fail.push(`a futex waiter returned ${c.r} after ${c.ms.toFixed(0)} ms (want 0, well under its 5000 ms bound)`);
  if (d.r !== 0 || d.ms > 1000) fail.push(`a waiter on a chained queue returned ${d.r} after ${d.ms.toFixed(0)} ms (want 0)`);
  const live = k.k_wq_live();
  if (fail.length) {
    for (const f of fail) console.error(`  ${f}`);
    console.error(`wait queue check FAILED: ${path}`);
    process.exit(1);
  }
  console.log(`  a write wakes the poll over two pipes ${late.toFixed(2)} ms later; a waiter on a third pipe slept its 600 ms out; a futex wake and a chained queue wake theirs (${k.k_ws_notifies() - notifies0} slot wakes, ${live} queues live)`);
  process.exit(0);
} else {
  const { module, memory, process_, slot, seq, ns } = workerData;
  const k = await instantiate(module, memory, process_);
  parentPort.postMessage('ready');
  const t0 = performance.now();
  const r = k.k_ws_park(slot, seq, BigInt(ns));
  // `performance.now()` counts from each thread's own origin.
  parentPort.postMessage({ r, ms: performance.now() - t0, end: performance.timeOrigin + performance.now() });
}
