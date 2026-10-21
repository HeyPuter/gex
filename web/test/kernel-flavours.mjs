// One kernel memory, processes of both memory flavours.
//
//   node web/test/kernel-flavours.mjs
//
// The global kernel module serves a process with a 32-bit memory and one
// with a memory64 (gex-kmem's memories 1 and 2, lazy-fetch.mjs
// `processMemories`): a page whose processes differ -- 64-bit Wine's wasm32 server
// beside a 64-bit memory's loader, the X server under `?mem64=1` -- has one
// kernel and one layout of its state. Two kernel builds, one per flavour,
// put their statics 16 bytes apart, and whichever started second read the
// first's state wrong (the server's Worker hung in its first kernel call).
//
// Checked in both start orders (the 32-bit memory's Worker first, then the
// 64-bit one's first, each on a fresh kernel memory): a pipe made by the
// first carries bytes both ways between the two address spaces, the 64-bit
// side's buffer above 4 GiB, and each side's checksum of its own memory.
import { Worker } from 'node:worker_threads';
import { readFileSync, existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
// Either build: the flavours are the same for both.
const KERNEL = [process.argv[2], 'gex_kernel.wasm', 'gex_kernel.x86.wasm']
  .filter(Boolean)
  .map((f) => join(HERE, '..', 'public', 'gex', f))
  .find((f) => existsSync(f));
if (!KERNEL) {
  console.log('skip kernel-flavours: run shims/kernel/build.sh first');
  process.exit(0);
}
const bytes = new Uint8Array(readFileSync(KERNEL));
const module = await WebAssembly.compile(bytes);

// A Worker with its own process memory of one flavour. The 64-bit one has
// a page above 4 GiB, where its buffer goes.
const WORKER = `
  import { parentPort, workerData } from 'node:worker_threads';
  const is64 = workerData.is64;
  const HIGH = 0x100000000;
  const proc = is64
    ? new WebAssembly.Memory({ address: 'i64', initial: BigInt(HIGH / 65536 + 2), maximum: 262144n, shared: true })
    : new WebAssembly.Memory({ initial: 64, maximum: 65535, shared: true });
  const empty = is64
    ? new WebAssembly.Memory({ initial: 0, maximum: 0, shared: true })
    : new WebAssembly.Memory({ address: 'i64', initial: 0n, maximum: 0n, shared: true });
  const k = (await WebAssembly.instantiate(workerData.module, {
    env: {
      memory: workerData.kernel,
      process: is64 ? empty : proc,
      process64: is64 ? proc : empty,
      lazy_fetch: () => -1,
    },
  })).exports;
  if (workerData.stackTop) k.__stack_pointer.value = workerData.stackTop | 0;
  const u8 = new Uint8Array(proc.buffer);
  const AT = is64 ? HIGH + 0x1000 : 0x40000;
  parentPort.on('message', (m) => {
    if (m.kind === 'put') {
      const b = new TextEncoder().encode(m.text);
      u8.set(b, AT);
      parentPort.postMessage({ n: k.k_pipe_write(m.id, BigInt(AT), b.length), sum: k.k_checksum(BigInt(AT), b.length) });
    } else if (m.kind === 'get') {
      const n = k.k_pipe_read(m.id, BigInt(AT), m.len);
      parentPort.postMessage({ n, text: n > 0 ? new TextDecoder().decode(u8.subarray(AT, AT + n)) : '' });
    } else {
      parentPort.postMessage({ value: Number(k[m.fn](...(m.args ?? []))) });
    }
  });
  parentPort.postMessage({ ready: true });
`;

const fail = (why) => {
  console.log(`FAIL kernel-flavours: ${why}`);
  process.exit(1);
};

async function round(firstIs64) {
  const kernel = new WebAssembly.Memory({ initial: 2048, maximum: 65536, shared: true });
  let stacks = null;
  let taken = 0;
  const spawn = (is64) => new Promise((resolve, reject) => {
    // The first Worker keeps the linked stack and says where the others go.
    const stackTop = stacks ? stacks.area + ++taken * stacks.size : 0;
    const w = new Worker(WORKER, { eval: true, workerData: { module, kernel, is64, stackTop } });
    const pending = [];
    const api = {
      worker: w,
      send: (msg) => new Promise((r) => { pending.push(r); w.postMessage(msg); }),
      call: (fn, ...args) => api.send({ fn, args }).then((m) => m.value),
    };
    w.on('message', (m) => (m.ready ? resolve(api) : pending.shift()?.(m)));
    w.on('error', reject);
  });
  const a = await spawn(firstIs64);
  stacks = { area: (await a.call('k_stack_area')) >>> 0, size: (await a.call('k_stack_size')) >>> 0 };
  const b = await spawn(!firstIs64);
  const order = firstIs64 ? '64-bit first' : '32-bit first';
  const id = await a.call('k_pipe_new', 1);
  if (id < 0) fail(`${order}: k_pipe_new gave ${id}`);
  for (const [from, to, text] of [[a, b, `from the first (${order})`], [b, a, `from the second (${order})`]]) {
    const put = await from.send({ kind: 'put', id, text });
    if (put.n !== text.length) fail(`${order}: wrote ${put.n} of ${text.length}`);
    if (!put.sum) fail(`${order}: the writer's checksum of its own buffer is 0`);
    if (!(await to.call('k_pipe_readable', id))) fail(`${order}: the other side does not see the bytes`);
    const got = await to.send({ kind: 'get', id, len: 256 });
    if (got.text !== text) fail(`${order}: read ${JSON.stringify(got.text)}, wrote ${JSON.stringify(text)}`);
  }
  await a.worker.terminate();
  await b.worker.terminate();
}

await round(false);
await round(true);
console.log('ok   kernel-flavours (a 32-bit and a 64-bit memory\'s process on one kernel, both start orders)');
