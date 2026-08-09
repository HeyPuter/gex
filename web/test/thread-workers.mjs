// A guest whose threads are Workers, on one shared address space.
//
//   node web/test/thread-workers.mjs <static-aarch64-program-that-uses-pthreads>
//
// This is the other half of "one Worker per process". A process gets a Worker
// and a memory of its own; its *threads* used to get neither -- `clone` put an
// entry in a table and the shim scheduled them cooperatively, one at a time,
// on the single wasm stack a Worker has. Correct, and never more than one core.
//
// A thread here is a second Worker on the memory the process already has.
// Nothing is copied: the shim's own state -- its heap, its statics, the
// `Host` that owns the file table and the memory map -- lives in that same
// linear memory, so a Worker that attaches to it finds the kernel already
// there. What cannot be shared is which *entry of the thread table* a Worker
// is running, and that is the one thing kept per Worker.
//
// Three things have to be true and each would break differently:
//
//   1. the program's answers are right -- the sums, the TLS, the condition
//      variable. A thread that ran on the wrong context or lost a futex wake
//      gets these wrong rather than crashing,
//   2. more than one Worker existed, or this tested the old path,
//   3. the process exits 0, which means every thread was joined.
import { Worker } from 'node:worker_threads';
import { readFileSync, existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { ProcessHost } from '../../shims/browser/js/host.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..', '..');
// The shared variants, and they have to agree: a memory import's `shared`
// flag is part of its type, so a plain shim will not instantiate against the
// memory a shared guest module wants, or the other way round.
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
  // What tells the shim its threads get Workers, and what makes the memory
  // one they can share.
  // The heartbeat says where each Worker is every so many safepoints. A run
  // with several Workers that stops has to say which one stopped and where,
  // or the only fact available is that nothing came out.
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

const status = await host.start({ elf, args: ['prog'] });

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
