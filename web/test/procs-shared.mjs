// fork, wait and kill across two Workers with separate address spaces.
//
//   node web/test/procs-shared.mjs
//
// This is what the snapshot switcher exists to fake, done properly.
//
// The switcher keeps every guest process in one address space, because that is
// the only way they can share a kernel: the process table, the exit statuses
// and the pending signals are ordinary Rust data structures, and Rust data
// structures live in a memory. Running a second process therefore means
// writing the first out and reading the second in -- 33 MiB a switch, and the
// address collisions that make it necessary in the first place.
//
// The way out is not a cheaper copy. It is to put the shared half somewhere
// that is not any one process's memory: the global kernel module, whose own
// memory every Worker instantiates against. `gex_kernel::ProcTable` moves in
// unchanged -- it was already written for this, with its state behind a lock
// and signals left for the target to collect rather than delivered by the
// sender.
//
// So this test has two Workers with *separate* process memories, standing in
// for two guest processes, and checks that between them they can do the three
// things the switcher is currently required for:
//
//   * one forks and the other is the child,
//   * the child exits and the parent's `wait4` collects the right status,
//   * one signals the other and the signal is there to be taken.
//
// If those hold, nothing about `fork`, `wait4` or `kill` needs the two
// processes to share an address space, and the switcher has no remaining job.
import { Worker } from 'node:worker_threads';
import { readFileSync, existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const KERNEL = join(HERE, '..', 'public', 'gex', 'gex_kernel.wasm');
if (!existsSync(KERNEL)) {
  console.log('skip procs-shared: run shims/kernel/build.sh first');
  process.exit(0);
}
const bytes = new Uint8Array(readFileSync(KERNEL));

/** The guest's address space in wasm pages, which the import declares. */
const GUEST_PAGES = 0xffff0000 / 65536;

// The one kernel memory, created once and handed to every Worker. Its maximum
// has to match what the module was linked with exactly, because a shared
// memory carries its bounds in its type.
const kernel = new WebAssembly.Memory({ initial: 2048, maximum: 16384, shared: true });

const WORKER = `
  import { parentPort, workerData } from 'node:worker_threads';
  // This Worker's own process memory. Not shared with the other Worker --
  // that separation is the point, and is what the switcher gives up.
  const proc = new WebAssembly.Memory({
    initial: workerData.guestPages, maximum: workerData.guestPages, shared: true,
  });
  const k = (await WebAssembly.instantiate(
    await WebAssembly.compile(workerData.bytes),
    { env: { memory: workerData.kernel, process: proc } },
  )).exports;
  parentPort.on('message', (m) => {
    const r = k[m.fn](...(m.args ?? []));
    parentPort.postMessage({ kind: 'ret', fn: m.fn, value: Number(r) });
  });
  parentPort.postMessage({ kind: 'ready' });
`;

function spawn() {
  return new Promise((resolve, reject) => {
    const w = new Worker(WORKER, {
      eval: true,
      workerData: { bytes, kernel, guestPages: GUEST_PAGES },
    });
    const pending = [];
    const api = {
      worker: w,
      call: (fn, ...args) =>
        new Promise((r) => {
          pending.push(r);
          w.postMessage({ fn, args });
        }),
    };
    w.on('message', (m) => {
      if (m.kind === 'ready') return resolve(api);
      pending.shift()?.(m.value);
    });
    w.on('error', reject);
  });
}

const fail = (why) => {
  console.log(`FAIL procs-shared: ${why}`);
  process.exit(1);
};

const parent = await spawn();
const child = await spawn();

// The parent forks. The pid comes back from the shared table, so the child's
// Worker can be told which process it is.
const pid = await parent.call('k_spawn', 1);
if (pid <= 1) fail(`fork gave pid ${pid}`);

// The other Worker sees it: a table in one process's memory would not have it.
if (!(await child.call('k_exists', pid))) {
  fail(`process ${pid} does not exist in the other Worker -- the table is not shared`);
}
if ((await child.call('k_ppid', pid)) !== 1) {
  fail(`process ${pid} has the wrong parent in the other Worker`);
}
if (!(await parent.call('k_has_children', 1))) fail('the parent has no children');

// A signal, left by one Worker and collected by the other. 2 is SIGINT, and
// the mask is `1 << (signo - 1)`.
if ((await parent.call('k_kill', 1, pid, 2)) !== 1) fail('kill signalled nothing');
if (!(await child.call('k_has_signals', pid))) {
  fail('the signal did not reach the other Worker');
}
const taken = await child.call('k_take_signals', pid);
if (taken !== 1 << 1) fail(`took ${taken}, expected ${1 << 1} for SIGINT`);

// The child exits, in its own Worker, and the parent collects it in the
// other. This is `wait4` across two address spaces, which is the call the
// switcher exists to make possible.
const status = 42 << 8; // WIFEXITED, code 42
await child.call('k_exit', pid, status);
const got = await parent.call('k_wait', 1, -1);
if (got !== pid) fail(`wait collected ${got}, expected ${pid}`);
const gotStatus = await parent.call('k_wait_status');
if (gotStatus !== status) fail(`wait status ${gotStatus}, expected ${status}`);

// And once collected it is gone, so a second wait says there is nothing --
// which is how a parent learns its children are finished rather than looping.
if ((await parent.call('k_wait', 1, -1)) !== -1) fail('the child was collected twice');
if (await parent.call('k_has_children', 1)) fail('the parent still has children');

parent.worker.terminate();
child.worker.terminate();
console.log('ok   procs-shared (fork, signal and wait across two address spaces)');
