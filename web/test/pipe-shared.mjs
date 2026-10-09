// A pipe whose two ends are in different address spaces.
//
//   node web/test/pipe-shared.mjs
//
// In the global kernel a pipe is a number: the buffer lives in the shared
// kernel memory, each process's file table holds an id, and the bytes cross by
// being copied through the process's own memory. So writer and reader can be
// Workers that share nothing else. (Under the old switcher a pipe was an
// `Arc<Pipe>` -- a pointer, hence a shared address space -- which was most of
// why it existed.)
//
// Checked: bytes written in one address space read intact in the other; a
// reader sees nothing before the write and everything after; closing the last
// writer is EOF, not a block, so `a | b` can finish.
import { Worker } from 'node:worker_threads';
import { readFileSync, existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const KERNEL = join(HERE, '..', 'public', 'gex', 'gex_kernel.wasm');
if (!existsSync(KERNEL)) {
  console.log('skip pipe-shared: run shims/kernel/build.sh first');
  process.exit(0);
}
const bytes = new Uint8Array(readFileSync(KERNEL));
const GUEST_PAGES = 0xffff0000 / 65536;
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
  // This Worker's own address space. The other Worker cannot see it, which is
  // the whole point -- the bytes have to travel through the kernel.
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
  const u8 = new Uint8Array(proc.buffer);
  const AT = 0x40000;

  parentPort.on('message', (m) => {
    if (m.kind === 'put') {
      // Stage the bytes in *this* process's memory, then hand the kernel an
      // address it cannot dereference.
      const b = new TextEncoder().encode(m.text);
      u8.set(b, AT);
      parentPort.postMessage({ kind: 'put', n: k.k_pipe_write(m.id, AT, b.length) });
    } else if (m.kind === 'get') {
      const n = k.k_pipe_read(m.id, AT, m.len);
      parentPort.postMessage({
        kind: 'get',
        n,
        text: n > 0 ? new TextDecoder().decode(u8.subarray(AT, AT + n)) : '',
      });
    } else {
      parentPort.postMessage({ kind: 'ret', value: Number(k[m.fn](...(m.args ?? []))) });
    }
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
      send: (msg) =>
        new Promise((r) => {
          pending.push(r);
          w.postMessage(msg);
        }),
      call: (fn, ...args) => api.send({ fn, args }).then((m) => m.value),
    };
    w.on('message', (m) => {
      if (m.kind === 'ready') return resolve(api);
      pending.shift()?.(m);
    });
    w.on('error', reject);
  });
}

const fail = (why) => {
  console.log(`FAIL pipe-shared: ${why}`);
  process.exit(1);
};

const writer = await spawn();
const reader = await spawn();

const id = await writer.call('k_pipe_new');
if (id < 0) fail(`k_pipe_new gave ${id}`);

// Nothing written yet, so the reader has nothing and would block. Both ends
// are open, so this is "not yet" rather than EOF.
if (await reader.call('k_pipe_buffered', id)) fail('the pipe had bytes before anything was written');
if (await reader.call('k_pipe_readable', id)) fail('an empty pipe with a live writer reported readable');

const message = 'through a pipe the two ends cannot both address';
const put = await writer.send({ kind: 'put', id, text: message });
if (put.n !== message.length) fail(`wrote ${put.n} of ${message.length}`);

// And the other address space reads it back.
if (!(await reader.call('k_pipe_readable', id))) fail('the reader does not see the written bytes');
const got = await reader.send({ kind: 'get', id, len: 256 });
if (got.text !== message) fail(`read ${JSON.stringify(got.text)}`);

// Drained again.
if (await reader.call('k_pipe_buffered', id)) fail('bytes remained after the read');

// The last writer closing is EOF, which is how `a | b` ends rather than
// hanging: readable becomes true and the read returns zero.
await writer.call('k_pipe_close_end', id, 1);
if (!(await reader.call('k_pipe_readable', id))) {
  fail('a pipe with no writers left is not readable, so a reader would block forever');
}
const eof = await reader.send({ kind: 'get', id, len: 256 });
if (eof.n !== 0) fail(`read ${eof.n} bytes at end of file, expected 0`);

writer.worker.terminate();
reader.worker.terminate();
console.log('ok   pipe-shared (a pipe between two address spaces, with a real EOF)');
