// One kernel, two Workers, two processes.
//
//   node web/test/kernel-shared.mjs
//
// The architecture that replaced the switcher, at its smallest. One global
// kernel module: every Worker instantiates it against the same shared memory,
// so the file table, process table and pipes are the same bytes and a syscall
// stays a direct call. Each Worker also hands the kernel its own process's
// memory, shared only with that process's threads -- which is why two processes
// can no longer collide on library addresses. A guest buffer thus lives where
// the kernel cannot address it, and `memory.copy` naming both memories reaches
// it.
//
// Checked here:
//   1. two Workers' kernel instances share state (a counter, and a note),
//   2. neither can see the other's process memory,
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

// The one shared memory, created once and handed to every Worker. Its maximum
// is the ceiling on everything the kernel will ever hold; its initial must
// cover the module's own data and stack.
const kernel = new WebAssembly.Memory({ initial: 2048, maximum: 16384, shared: true });

const WORKER = `
  import { parentPort, workerData } from 'node:worker_threads';
  // Each Worker builds its own process memory: shared (its threads are Workers
  // on it), but scoped to this process -- the kernel's memory spans every
  // process, this one only this process's threads. The full guest address
  // space, because the import's bounds are part of its type; reserved, not
  // committed, so a process costs what it touches.
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

// 2. The process memories are not shared: B's 0x1000 is untouched. If this
//    fails, two guests share an address space and the separation is imaginary.
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
