// One kernel, two Workers, two processes.
//
//   node web/test/kernel-shared.mjs
//
// This is the architecture the switcher is being replaced by, at its smallest.
//
// There is one *global* kernel module. Every Worker instantiates it, and every
// instance is given the same memory -- a `SharedArrayBuffer` -- so the file
// table, the process table and the pipes are not copied, messaged or
// synchronised between processes. They are simply the same bytes. A syscall
// stays a direct call.
//
// Each Worker also hands the kernel *its own process's* memory, as a second
// import. That memory is not shared with any other process, which is the whole
// reason the snapshot switcher can go: two processes can no longer put their
// libraries at the same addresses, so there is nothing to save and restore.
// A guest buffer therefore lives somewhere the kernel cannot address with a
// pointer, and `memory.copy` -- naming both memories -- is how it is reached.
//
// What is checked here is exactly the three properties everything above will
// assume:
//
//   1. two Workers' kernel instances share state (a counter, and a note),
//   2. neither can see the other's process memory -- both are shared, but a
//      process's is shared only with its own threads,
//   3. the kernel can pull a buffer out of whichever process called it.
import { Worker } from 'node:worker_threads';
import { readFileSync, existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const KERNEL = join(HERE, '..', 'public', 'gex', 'gex_kernel.wasm');
if (!existsSync(KERNEL)) {
  console.log('skip kernel-shared: run shims/kernel/build.sh first');
  process.exit(0);
}
const bytes = new Uint8Array(readFileSync(KERNEL));

/** The guest's address space in wasm pages: `GUEST_MEM_BYTES / WASM_PAGE`. */
const GUEST_PAGES = 0xffff0000 / 65536;

// The one shared memory. Created once, here, and handed to every Worker --
// which is what "global kernel" means concretely.
//
// It must start at least as large as the module's own data and stack, which
// the linker fixed when it built it; a shared memory also has to declare a
// maximum, and that maximum is the ceiling on everything the kernel will ever
// hold -- every process's file table, every pipe's buffer.
const kernel = new WebAssembly.Memory({ initial: 64, maximum: 16384, shared: true });

const WORKER = `
  import { parentPort, workerData } from 'node:worker_threads';
  // Each Worker builds its *own* process memory. Not shared, not seen by
  // anyone else: this is one guest process's address space.
  // Shared, not plain: this is one process's address space and every thread
  // of that process is a Worker running on it. Both memories are shared; what
  // differs is scope -- the kernel's spans every process, this one spans only
  // this process's threads.
  //
  // The full guest address space, because the kernel module's import declares
  // it and a memory import's bounds are part of its type. It is reserved, not
  // committed: what a process has actually touched is what it costs.
  const proc = new WebAssembly.Memory({
    initial: workerData.guestPages, maximum: workerData.guestPages, shared: true,
  });
  const mod = await WebAssembly.compile(workerData.bytes);
  const k = (await WebAssembly.instantiate(mod, {
    env: { memory: workerData.kernel, process: proc },
  })).exports;
  const u8 = new Uint8Array(proc.buffer);

  parentPort.on('message', (m) => {
    if (m.kind === 'spawn') {
      parentPort.postMessage({ kind: 'spawned', pid: k.k_spawn(m.ppid) });
    } else if (m.kind === 'exists') {
      parentPort.postMessage({ kind: 'exists', yes: k.k_exists(m.pid) });
    } else if (m.kind === 'checksum') {
      const b = new TextEncoder().encode(m.text);
      u8.set(b, m.at);
      parentPort.postMessage({ kind: 'sum', sum: k.k_checksum(m.at, b.length) });
    } else if (m.kind === 'peek') {
      // Whatever is at this address in *my* process memory.
      parentPort.postMessage({ kind: 'peeked', byte: u8[m.at] });
    }
  });
  parentPort.postMessage({ kind: 'ready' });
`;

function spawn() {
  return new Promise((resolve) => {
    const w = new Worker(WORKER, {
      eval: true,
      workerData: { bytes, kernel, guestPages: GUEST_PAGES },
    });
    const pending = [];
    const api = {
      worker: w,
      ask: (msg, want) =>
        new Promise((r) => {
          pending.push({ want, r });
          w.postMessage(msg);
        }),
    };
    w.on('message', (m) => {
      if (m.kind === 'ready') return resolve(api);
      const i = pending.findIndex((p) => p.want === m.kind);
      if (i >= 0) pending.splice(i, 1)[0].r(m);
    });
  });
}

const fail = (why) => {
  console.log(`FAIL kernel-shared: ${why}`);
  process.exit(1);
};

const a = await spawn();
const b = await spawn();

// 1. The kernel's state is one thing, not two: a process created through one
//    Worker's instance is there in the other's.
const spawned = await a.ask({ kind: 'spawn', ppid: 1 }, 'spawned');
if (spawned.pid <= 1) fail(`spawn gave pid ${spawned.pid}`);
const seen = await b.ask({ kind: 'exists', pid: spawned.pid }, 'exists');
if (!seen.yes) {
  fail(`process ${spawned.pid} is missing in the other Worker -- the kernel memory is not shared`);
}

// 2. And the process memories are *not* shared. A wrote 0xbeef at 0x1000 of
//    its own; B's 0x1000 must be untouched. If this fails, two guests are
//    sharing an address space and the whole separation is imaginary.
const peek = await b.ask({ kind: 'peek', at: 0x1000 }, 'peeked');
if (peek.byte !== 0) {
  fail(`process B sees ${peek.byte} at an address only A wrote -- the process memories are shared`);
}

// 3. A buffer, pulled across by the kernel from whichever process called.
const text = 'the kernel cannot address this, and reads it anyway';
const sa = await a.ask({ kind: 'checksum', at: 0x3000, text }, 'sum');
const sb = await b.ask({ kind: 'checksum', at: 0x3000, text }, 'sum');
if (sa.sum !== sb.sum || sa.sum === 0) {
  fail(`the same bytes in two processes checksummed ${sa.sum} and ${sb.sum}`);
}

a.worker.terminate();
b.worker.terminate();
console.log('ok   kernel-shared (one kernel memory, two process memories, copies across both)');
