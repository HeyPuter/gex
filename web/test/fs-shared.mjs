// A filesystem two address spaces both see.
//
//   node web/test/fs-shared.mjs
//
// The files live in the global kernel; each process mounts a handle onto them.
// Checked here: a file written through one process's memory and read back
// through another's, the bytes crossing a memory neither of them owns. Under
// the old snapshot switcher the child inherited the parent's whole address
// space, so this question never came up.
import { Worker } from 'node:worker_threads';
import { readFileSync, existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const KERNEL = join(HERE, '..', 'public', 'gex', 'gex_kernel.wasm');
if (!existsSync(KERNEL)) {
  console.log('skip fs-shared: run shims/kernel/build.sh first');
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
  // This Worker's own address space. The other cannot see it, which is the
  // whole point: a path and its contents have to travel through the kernel.
  const proc = new WebAssembly.Memory({
    initial: workerData.guestPages, maximum: workerData.guestPages, shared: true,
  });
  const k = (await WebAssembly.instantiate(
    await WebAssembly.compile(workerData.bytes),
    { env: { memory: workerData.kernel, process: proc } },
  )).exports;
  // This Worker's own stack. A wasm global belongs to an instance, but wasm-ld
  // starts every one at the same shared address.
  if (workerData.stackTop) k.__stack_pointer.value = workerData.stackTop | 0;
  const u8 = new Uint8Array(proc.buffer);
  const enc = new TextEncoder();
  const dec = new TextDecoder();
  // Two staging areas in this process's memory: one for a path, one for the
  // bytes. Fixed addresses because nothing else is running in here.
  const PATH = 0x40000, DATA = 0x50000, OUT = 0x60000;
  const put = (at, b) => { u8.set(b, at); return [at, b.length]; };
  const path = (s) => put(PATH, enc.encode(s));

  parentPort.on('message', (m) => {
    const r = {};
    if (m.kind === 'write') {
      const [pa, pn] = path(m.path);
      const [da, dn] = put(DATA, enc.encode(m.text));
      // flags is an i64 in the module type, so it must be a BigInt: a Number
      // throws inside the Worker, which reads from outside as a hang.
      const h = k.k_fs_open(pa, pn, BigInt(m.flags ?? 0o101), 0o644); // O_WRONLY|O_CREAT
      r.handle = h;
      if (h >= 0) {
        r.wrote = k.k_fs_write(h, 0n, da, dn);
        k.k_fs_close(h);
      }
    } else if (m.kind === 'read') {
      const [pa, pn] = path(m.path);
      const h = k.k_fs_open(pa, pn, 0n, 0);
      r.handle = h;
      if (h >= 0) {
        const n = k.k_fs_read(h, 0n, OUT, 256);
        r.n = n;
        r.text = n > 0 ? dec.decode(u8.subarray(OUT, OUT + n)) : '';
        k.k_fs_close(h);
      }
    } else if (m.kind === 'meta') {
      const [pa, pn] = path(m.path);
      r.rc = k.k_fs_path_meta(pa, pn, OUT);
      if (r.rc === 0) {
        const dv = new DataView(proc.buffer, OUT, 32);
        r.type = dv.getUint32(0, true);
        r.mode = dv.getUint32(4, true);
        r.size = Number(dv.getBigUint64(8, true));
      }
    } else if (m.kind === 'mkdir') {
      const [pa, pn] = path(m.path);
      r.rc = k.k_fs_mkdir(pa, pn, 0o755);
    } else if (m.kind === 'list') {
      const [pa, pn] = path(m.path);
      const h = k.k_fs_open(pa, pn, 0n, 0);
      r.handle = h;
      if (h >= 0) {
        const n = k.k_fs_read_dir(h, OUT, 4096);
        r.n = n;
        r.names = [];
        const dv = new DataView(proc.buffer);
        for (let i = OUT; i + 16 <= OUT + Math.max(n, 0); ) {
          const len = dv.getUint32(i, true);
          r.names.push(dec.decode(u8.subarray(i + 16, i + 16 + len)));
          i += 16 + len;
        }
        k.k_fs_close(h);
      }
    } else if (m.kind === 'add') {
      const [pa, pn] = path(m.path);
      const [da, dn] = put(DATA, enc.encode(m.text));
      r.rc = k.k_fs_add(pa, pn, da, dn);
    }
    parentPort.postMessage(r);
  });
  parentPort.postMessage({ ready: true });
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
    };
    w.on('message', (m) => {
      if (m.ready) return resolve(api);
      pending.shift()?.(m);
    });
    w.on('error', reject);
  });
}

const fail = (why) => {
  console.log(`FAIL fs-shared: ${why}`);
  process.exit(1);
};

const a = await spawn();
const b = await spawn();

// The page's bundle, put in by whoever loads first.
const bundled = 'the interpreter a forked child has to find';
const added = await a.send({ kind: 'add', path: '/lib/ld.so', text: bundled });
if (added.rc !== 0) fail(`k_fs_add gave ${added.rc}`);

// And the other address space reads it, having never seen those bytes.
const got = await b.send({ kind: 'read', path: '/lib/ld.so' });
if (got.text !== bundled) fail(`the second process read ${JSON.stringify(got.text)}`);

// A file one process writes at run time, read by the other. This is what a
// shell redirecting into a file and a child reading it back needs.
const message = 'written through one memory and read through another';
const wrote = await a.send({ kind: 'write', path: '/tmp/note', text: message });
if (wrote.handle < 0) fail(`open for write gave ${wrote.handle}`);
if (wrote.wrote !== message.length) fail(`wrote ${wrote.wrote} of ${message.length}`);

const back = await b.send({ kind: 'read', path: '/tmp/note' });
if (back.text !== message) fail(`read back ${JSON.stringify(back.text)}`);

// Metadata, which is how a guest decides whether to open something at all.
const meta = await b.send({ kind: 'meta', path: '/tmp/note' });
if (meta.rc !== 0) fail(`stat gave ${meta.rc}`);
if (meta.size !== message.length) fail(`stat says ${meta.size} bytes, wrote ${message.length}`);
if (meta.type !== 0) fail(`stat says type ${meta.type}, expected a regular file`);

// A directory made in one process is a directory in the other, and lists what
// the other put in it. `ld.so` was added by A; B has to see the parent too.
if ((await b.send({ kind: 'mkdir', path: '/tmp/sub' })).rc !== 0) fail('mkdir failed');
await b.send({ kind: 'write', path: '/tmp/sub/one', text: 'x' });
const listed = await a.send({ kind: 'list', path: '/tmp/sub' });
if (!listed.names?.includes('one')) {
  fail(`the other process's directory listed ${JSON.stringify(listed.names)}`);
}

a.worker.terminate();
b.worker.terminate();
console.log('ok   fs-shared (one filesystem, two address spaces, bytes crossing both)');
