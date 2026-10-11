/**
 * Does a futex wake reach a Worker parked on that futex, well before the
 * park's bound?
 *
 * The page's Workers park on a futex with the kernel module's
 * `k_park_futex(addr, gen, ns)`: a wait on that futex's generation slot,
 * bounded by `ns`. `k_futex_wake(addr)` bumps the slot and must notify it.
 * When it notified only the global wake word, every futex park slept out its
 * whole bound (20 ms in W3's `gex_park`), and Undertale in the page lost a
 * third of its frame rate once nothing else cut those parks short.
 *
 * This instantiates the kernel module in two threads on shared memory. The
 * second thread parks on a futex for 5 s; the first wakes it after 100 ms.
 * The park must end, woken (0), within 1 s. A parked waiter on another
 * address must not be woken by it unless the two share a slot.
 *
 * Usage: node check-futex-wake.mjs [gex_kernel.wasm]
 */
import { readFile } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Worker, isMainThread, workerData, parentPort } from 'node:worker_threads';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..', '..', '..');

async function instantiate(module, memory, process_) {
  const inst = await WebAssembly.instantiate(module, {
    env: { memory, process: process_, lazy_fetch: () => 0 },
  });
  return inst.exports;
}

if (isMainThread) {
  const path = process.argv[2] || join(ROOT, 'web', 'public', 'gex', 'gex_kernel.wasm');
  const module = await WebAssembly.compile(await readFile(path));
  const memory = new WebAssembly.Memory({ initial: 2048, maximum: 65536, shared: true });
  const process_ = new WebAssembly.Memory({ initial: 1, maximum: 65535, shared: true });
  const k = await instantiate(module, memory, process_);
  const ADDR = 0x1234_5670n;
  const OTHER = 0x0765_4320n;
  const park = (addr, ms) => new Promise((resolve, reject) => {
    const w = new Worker(fileURLToPath(import.meta.url), {
      workerData: { module, memory, process_, addr, gen: k.k_futex_gen(addr), ns: ms * 1e6 },
    });
    w.once('message', resolve);
    w.once('error', reject);
  });
  // The kernel's `futex_slot` (16,384 slots): two addresses may share one.
  const slot = (addr) => {
    const a = addr >> 2n;
    return Number((BigInt.asUintN(64, (a ^ (a >> 15n)) * 0x9E3779B97F4A7C15n) >> 40n) & 16383n);
  };
  const sameSlot = slot(ADDR) === slot(OTHER);
  const woken = park(ADDR, 5000);
  const other = park(OTHER, 600);
  await new Promise((r) => setTimeout(r, 100));
  k.k_futex_wake(ADDR);
  const [a, b] = await Promise.all([woken, other]);
  const fail = [];
  if (a.r !== 0 || a.ms > 1000) fail.push(`the woken park returned ${a.r} after ${a.ms.toFixed(0)} ms (want 0, well under its 5000 ms bound)`);
  if (!sameSlot && (b.r !== 2 || b.ms < 500)) fail.push(`a park on another futex returned ${b.r} after ${b.ms.toFixed(0)} ms (want its 600 ms timeout, 2)`);
  if (fail.length) {
    for (const f of fail) console.error(`  ${f}`);
    console.error(`futex wake check FAILED: ${path}`);
    process.exit(1);
  }
  console.log(`  a futex wake ends a parked Worker's wait (after ${a.ms.toFixed(0)} ms; the wake came 100 ms after the Worker started); another futex's waiter sleeps on`);
  process.exit(0);
} else {
  const { module, memory, process_, addr, gen, ns } = workerData;
  const k = await instantiate(module, memory, process_);
  const t0 = performance.now();
  const r = k.k_park_futex(addr, gen, BigInt(ns));
  parentPort.postMessage({ r, ms: performance.now() - t0 });
}
