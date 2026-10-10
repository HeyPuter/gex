// fork, wait and kill across two Workers with separate address spaces.
//
//   node web/test/procs-shared.mjs
//
// What the snapshot switcher existed to fake, done properly. The shared half --
// `gex_kernel::ProcTable`, exit statuses, pending signals -- lives in the
// global kernel's own memory that every Worker instantiates against, so two
// processes with separate memories can still fork, wait and signal each other.
// The switcher had kept them in one address space (33 MiB a switch) only so
// they could share that state.
//
// Checked: one forks and the other is the child; the child exits and the
// parent's `wait4` collects the right status; one signals the other and the
// signal is there to be taken.
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

// Where the kernel's per-Worker stacks are. Probed from a throwaway instance
// here, before any Worker exists; each Worker below installs its own slice.
const probe = (await WebAssembly.instantiate(
  await WebAssembly.compile(bytes),
  {
    env: {
      memory: kernel,
      lazy_fetch: () => -1,
      process: new WebAssembly.Memory({
        initial: GUEST_PAGES, maximum: GUEST_PAGES, shared: true,
      }),
    },
  },
)).exports;
const STACK_AREA = probe.k_stack_area() >>> 0;
const STACK_SIZE = probe.k_stack_size() >>> 0;
let stacksTaken = 0;

const WORKER = `
  import { parentPort, workerData } from 'node:worker_threads';
  // This Worker's own process memory, not shared with the other Worker -- that
  // separation is the point, and is what the switcher gave up.
  const proc = new WebAssembly.Memory({
    initial: workerData.guestPages, maximum: workerData.guestPages, shared: true,
  });
  const k = (await WebAssembly.instantiate(
    await WebAssembly.compile(workerData.bytes),
    { env: { memory: workerData.kernel, process: proc, lazy_fetch: () => -1 } },
  )).exports;
  // This Worker's own stack. A wasm global belongs to an instance, but wasm-ld
  // starts every one at the same shared address.
  if (workerData.stackTop) k.__stack_pointer.value = workerData.stackTop | 0;
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
      workerData: {
        bytes,
        kernel,
        guestPages: GUEST_PAGES,
        stackTop: STACK_AREA + ++stacksTaken * STACK_SIZE,
      },
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

// The child exits in its own Worker; the parent collects it in the other --
// `wait4` across two address spaces. `k_exit` takes an exit code; `k_wait`
// packs it into the word a parent reads, with the pid above it.
await child.call('k_exit', pid, 42);
if (!(await parent.call('k_wait_ready', 1, -1))) fail('a finished child was not ready to collect');
const packed = BigInt(await parent.call('k_wait', 1, -1));
const got = Number(packed >> 32n);
const gotStatus = Number(packed & 0xffffffffn);
if (got !== pid) fail(`wait collected ${got}, expected ${pid}`);
if (gotStatus !== 42 << 8) fail(`wait status ${gotStatus}, expected ${42 << 8}`);

// And once collected it is gone, so a second wait says there is nothing --
// which is how a parent learns its children are finished rather than looping.
if ((await parent.call('k_wait', 1, -1)) !== 0) fail('the child was collected twice');
if (await parent.call('k_has_children', 1)) fail('the parent still has children');

parent.worker.terminate();
child.worker.terminate();
console.log('ok   procs-shared (fork, signal and wait across two address spaces)');
