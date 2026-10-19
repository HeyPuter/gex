/**
 * Does page input reach a process whose every thread is parked, well before
 * the park's bound?
 *
 * A Wayland client's input goes page -> compositor -> the process's ring in
 * the wl arena (`sendEvent`) -> the shim's compositor pump, which runs on a
 * scheduler turn of one of the process's Workers. With every Worker parked on
 * its wait slot (`gex_park`), nothing drains the ring unless `sendEvent`
 * notifies the process's wait queue, which the process's `WlProxy` names and
 * every waiting thread of the process is registered on.
 *
 * This instantiates the kernel module in a second thread on shared memory and
 * parks it on a slot registered on a queue for 5 s (`k_ws_park`, as
 * `gex_park` does); the first
 * thread claims a block as a guest would, then sends a pointer event 100 ms
 * later. The park must end, woken (0), within a few milliseconds of the event.
 *
 * Usage: node check-wl-input-wake.mjs [gex_kernel.wasm]
 */
import { readFile } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Worker, isMainThread, workerData, parentPort } from 'node:worker_threads';
import { makeWlChannel, WlProxy, sendEvent, setKernelMemory } from './wl-bridge.mjs';

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

  // The guest's half: a process claims its block (on its first publish).
  const channel = makeWlChannel();
  const proxy = new WlProxy(channel, () => {}, () => 7);
  const queue = k.k_wq_new();
  const slot = k.k_ws_alloc();
  k.k_wq_add(queue, slot);
  proxy.wakeAddr = () => ({ addr: k.k_wq_addr(queue), slots: k.k_ws_addr(0) });
  const guestMem = new WebAssembly.Memory({ initial: 1, maximum: 16, shared: true });
  proxy.publish(guestMem, 0, 0);
  if (!proxy.ok) {
    console.error('wl input wake check FAILED: the proxy claimed no block');
    process.exit(1);
  }
  // The compositor's half, in its own realm in the page: its own view of the
  // arena, and the kernel memory a process's `wl-memory` message carried.
  const compositorChannel = { arena: channel.arena };
  setKernelMemory(compositorChannel, memory);

  const seq = k.k_ws_seq(slot);
  // The event goes once the Worker is about to park, not at a fixed time
  // after it starts: a Worker instantiating the kernel can take longer than
  // that (Node 24 under a gate's load), and an event sent before the park
  // reads as a park that never waited (`atomic.wait` says 1, "not equal").
  let ready;
  const parking = new Promise((r) => { ready = r; });
  const parked = new Promise((resolve, reject) => {
    const w = new Worker(fileURLToPath(import.meta.url), {
      workerData: { module, memory, process_, slot, seq, ns: 5000 * 1e6 },
    });
    w.on('message', (m) => (m === 'parking' ? ready() : resolve(m)));
    w.once('error', reject);
  });
  await parking;
  await new Promise((r) => setTimeout(r, 100));
  const sent = performance.timeOrigin + performance.now();
  // Block 0: the first block a lone process claims.
  sendEvent(compositorChannel, 0, 0 /* motion */, 10, 20, 1, 0, 1, 1);
  const r = await parked;
  const late = r.end - sent;
  if (r.r !== 0 || late > 10) {
    console.error(`  the parked Worker returned ${r.r}, ${late.toFixed(1)} ms after the event (want 0, within 10 ms)`);
    console.error(`wl input wake check FAILED: ${path}`);
    process.exit(1);
  }
  console.log(`  page input wakes a process whose threads are all parked (${late.toFixed(1)} ms after the event; the park's bound was 5 s)`);
  process.exit(0);
} else {
  const { module, memory, process_, slot, seq, ns } = workerData;
  const k = await instantiate(module, memory, process_);
  parentPort.postMessage('parking');
  const r = k.k_ws_park(slot, seq, BigInt(ns));
  // `performance.now()` counts from each thread's own origin.
  parentPort.postMessage({ r, end: performance.timeOrigin + performance.now() });
}
