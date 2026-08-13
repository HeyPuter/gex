// A guest whose threads are Workers, on one shared address space.
//
//   node web/test/thread-workers.mjs <static-aarch64-program-that-uses-pthreads>
//
// The other half of "one Worker per process": a thread is a second Worker on
// the memory the process already has. The shim's state (heap, statics, the
// `Host` owning the file table and memory map) lives in that memory, so an
// attaching Worker finds the kernel there; only which thread-table entry it
// runs is kept per Worker. (Threads used to be scheduled cooperatively on one
// Worker's single wasm stack -- correct, but never more than one core.)
//
// Checked, each breaking differently:
//   1. the program's answers -- sums, TLS, condition variable -- are right,
//   2. more than one Worker existed, or this tested the old path,
//   3. the process exits 0, so every thread was joined.
import { Worker } from 'node:worker_threads';
import { readFileSync, existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { ProcessHost } from '../../shims/browser/js/host.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..', '..');
// The shared variants: the `shared` flag is part of a memory import's type, so
// a plain shim will not instantiate against a shared guest module.
const SHIM = join(ROOT, 'target/wasm32-unknown-unknown/release/gex_browser.shared.wasm');
const SEED = join(ROOT, 'web/public/gex/seed.shared.wasm');
const KERNEL = join(ROOT, 'web/public/gex/gex_kernel.wasm');
const WORKER = join(ROOT, 'shims/browser/js/worker.mjs');

const [elfPath] = process.argv.slice(2);
for (const p of [SHIM, SEED, KERNEL, elfPath]) {
  if (!p || !existsSync(p)) {
    console.log(`skip thread-workers: missing ${p ?? 'a program that uses pthreads'}`);
    process.exit(0);
  }
}

const shim = new Uint8Array(readFileSync(SHIM));
const seed = new Uint8Array(readFileSync(SEED));
const elf = new Uint8Array(readFileSync(elfPath));
const kernel = {
  module: await WebAssembly.compile(new Uint8Array(readFileSync(KERNEL))),
  memory: new WebAssembly.Memory({ initial: 2048, maximum: 16384, shared: true }),
};

let output = '';
const host = new ProcessHost({
  makeWorker: () => new Worker(new URL(`file://${WORKER}`)),
  shim,
  guest: seed,
  kernel,
  // `shared` tells the shim its threads get Workers on a shared memory. The
  // heartbeat reports where each Worker is every so many safepoints, so a run
  // that stops can say which Worker stopped.
  extra: {
    shared: true,
    debugExit: !!process.env.GEX_DEBUG_EXIT,
    heartbeat: process.env.GEX_HEARTBEAT ? Number(process.env.GEX_HEARTBEAT) : undefined,
  },
  onOutput: (fd, text) => {
    output += text;
    process.stdout.write(text);
  },
});

const status = await host.start({
  elf,
  args: ['prog'],
  opts: { strace: !!process.env.GEX_STRACE },
});

const want = [
  ['tls ok', /tls ok/],
  ['the guarded counter', /guarded (\d+) expected \1\b/],
  ['the per-thread sums', /sum (\d+) expected \1\b/],
  ['the condition variable', /condvar consumed 100/],
  ['every thread joined', /^done$/m],
];
const missing = want.filter(([, re]) => !re.test(output)).map(([what]) => what);

if (missing.length || status !== 0) {
  console.log(`FAIL thread-workers: ${missing.join(', ') || `exit ${status}`}`);
  console.log(`     ${host.threads} thread Worker(s), exit ${status}`);
  process.exit(1);
}
if (!host.threads) {
  console.log('FAIL thread-workers: no thread ever got a Worker, so the old path was tested');
  process.exit(1);
}
console.log(`ok   thread-workers (${host.threads} threads, each in a Worker of its own)`);
process.exit(0);
