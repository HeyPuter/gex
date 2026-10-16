// Two Workers on one process's memory: do they share a kernel?
//
//   node web/test/threads-shared.mjs
//
// A thread is a second Worker on the memory a process already has. The shim's
// own state -- heap, statics, the `Host` owning the file and process tables --
// lives in that linear memory, so two Workers on it should see one kernel. That
// needed `HOST` to stop being a thread-local: with `+atomics` each Worker gets
// its own TLS block, so a thread-local kernel would silently fork.
//
// Checked, worst break first:
//   1. a second Worker can instantiate against a memory in use -- re-running
//      the data segments would wipe the first Worker's kernel,
//   2. the second Worker sees the guest's memory as the first left it,
//   3. the second Worker sees the first's kernel state: a file created through
//      one is open through the other.
import { Worker } from 'node:worker_threads';
import { readFileSync, existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..', '..');
const SHIM = join(ROOT, 'target/wasm32-unknown-unknown/release/gex_browser.shared.wasm');
const SEED = join(ROOT, 'web/public/gex/seed.shared.wasm');
const GEX = join(ROOT, 'shims/browser/js/gex.mjs');

for (const p of [SHIM, SEED]) {
  if (!existsSync(p)) {
    console.log(`skip threads-shared: no ${p} (needs the shared-memory build)`);
    process.exit(0);
  }
}

// tests/integration/programs/threads-shared.c: it clones a thread first,
// since only a threaded process keeps what it translates for a Worker that
// joins it (`Jit::ever_threaded`), then runs code it has not run before.
const [elfPath] = process.argv.slice(2);
if (!elfPath || !existsSync(elfPath)) {
  console.log('skip threads-shared: give it tests/integration/programs/threads-shared.c, built static for aarch64');
  process.exit(0);
}

// The process's address space. One memory, two Workers -- that is the whole
// definition of a thread here. Made as the runtime makes a process's (gex.mjs:
// 112 MiB to start, growing to 4 GiB less a page): made whole at 4 GiB, its
// heap arena had nowhere to grow (at 0xffff0000, above its ceiling) and the
// shim's stack slots were never set up.
const memory = new WebAssembly.Memory({ initial: 0x07000000 / 65536, maximum: 0xffff0000 / 65536, shared: true });

const shim = new Uint8Array(readFileSync(SHIM));
const seed = new Uint8Array(readFileSync(SEED));
const elf = new Uint8Array(readFileSync(elfPath));

const WORKER = `
  import { parentPort, workerData } from 'node:worker_threads';
  const { Gex, Env } = await import(${JSON.stringify(GEX)});
  class E extends Env {
    write(fd, bytes) {
      parentPort.postMessage({ kind: 'out', text: new TextDecoder().decode(bytes.slice()) });
    }
    now() { return BigInt(Date.now()) * 1000000n; }
    // The thread the program clones. Nothing runs it (it would only exit);
    // its record carries the process's module store, which the second
    // Worker joins as a thread's Worker does.
    spawnThread(t) { parentPort.postMessage({ kind: 'thread', tid: t.tid, moduleStore: t.moduleStore }); }
  }
  const gex = new Gex(new E(), { memory: workerData.memory, moduleStore: workerData.moduleStore ?? null });
  let entry = null;
  parentPort.on('message', async (m) => {
    try {
      if (m.kind === 'load') {
        // Only the first Worker loads a program. The second adopts the memory
        // and instantiates against it, which is what a thread does.
        entry = await gex.load(workerData.shim, workerData.seed, workerData.elf,
                               m.files ?? {}, [ 'prog' ], {});
        parentPort.postMessage({ kind: 'loaded', entry: String(entry) });
      } else if (m.kind === 'attach') {
        await gex.attach(workerData.shim, workerData.seed);
        parentPort.postMessage({ kind: 'attached' });
      } else if (m.kind === 'peek') {
        const u8 = new Uint8Array(gex.memory.buffer);
        parentPort.postMessage({ kind: 'peeked', bytes: Array.from(u8.subarray(m.at, m.at + m.len)) });
      } else if (m.kind === 'poke') {
        new Uint8Array(gex.memory.buffer).set(new Uint8Array(m.bytes), m.at);
        parentPort.postMessage({ kind: 'poked' });
      } else if (m.kind === 'run') {
        // Running translates blocks and installs them into *this* Worker's
        // dispatch tables. A thread joining afterwards has none of them.
        const status = await gex.run(entry);
        // The shim's registry of translated modules: \`installedModules\`
        // counts those a Worker installed by catching up, which a Worker
        // that translated them itself never does.
        const registry = gex.shim?.exports?.gex_module_count?.();
        parentPort.postMessage({
          kind: 'ran', status, modules: registry === undefined ? gex.installedModules : registry >>> 0,
        });
      } else if (m.kind === 'modules') {
        parentPort.postMessage({ kind: 'modules', n: gex.installedModules });
      } else if (m.kind === 'file') {
        const b = gex.readFile(m.path);
        parentPort.postMessage({ kind: 'file', text: b ? new TextDecoder().decode(b) : null });
      }
    } catch (err) {
      parentPort.postMessage({ kind: 'threw', where: m.kind, message: String(err && err.message || err) });
    }
  });
  parentPort.postMessage({ kind: 'ready' });
`;

let thread = null;

function spawn(moduleStore) {
  return new Promise((resolve) => {
    const w = new Worker(WORKER, {
      eval: true,
      workerData: { shim, seed, elf, memory, moduleStore },
    });
    const pending = [];
    const api = {
      worker: w,
      out: '',
      ask: (msg, want) =>
        new Promise((r) => {
          pending.push({ want, r });
          w.postMessage(msg);
        }),
    };
    w.on('message', (m) => {
      if (m.kind === 'ready') return resolve(api);
      if (m.kind === 'out') {
        api.out += m.text;
        return;
      }
      if (m.kind === 'thread') {
        thread = m;
        return;
      }
      if (m.kind === 'threw') {
        console.log(`FAIL threads-shared: worker threw during ${m.where}: ${m.message}`);
        process.exit(1);
      }
      const i = pending.findIndex((p) => p.want === m.kind);
      if (i >= 0) pending.splice(i, 1)[0].r(m);
    });
    w.on('error', (err) => {
      console.log(`FAIL threads-shared: worker error: ${err.message}`);
      process.exit(1);
    });
  });
}

const a = await spawn();
await a.ask({ kind: 'load', files: { '/hello': new TextEncoder().encode('written by thread A') } }, 'loaded');

// A marker in the guest's address space, from the first Worker.
const marker = [...new TextEncoder().encode('thread A was here')];
await a.ask({ kind: 'poke', at: 0x900000, bytes: marker }, 'poked');

// Run it, which is what fills the first Worker's dispatch tables.
const ran = await a.ask({ kind: 'run' }, 'ran');
if (ran.status !== 0 || !/^threads-shared \d+$/m.test(a.out)) {
  console.log(`FAIL threads-shared: the program exited ${ran.status}: ${JSON.stringify(a.out)}`);
  process.exit(1);
}
if (!thread) {
  console.log('FAIL threads-shared: the program cloned no thread');
  process.exit(1);
}
// A threaded process keeps what it translated: a registry with nothing in it
// means the replay below checks nothing.
if (!ran.modules) {
  console.log('FAIL threads-shared: the threaded process kept no translated modules for a joining Worker');
  process.exit(1);
}

// 1 and 2: a second Worker on the same memory, seeing what the first wrote.
const b = await spawn(thread.moduleStore);
await b.ask({ kind: 'attach' }, 'attached');
const seen = await b.ask({ kind: 'peek', at: 0x900000, len: marker.length }, 'peeked');
const text = new TextDecoder().decode(new Uint8Array(seen.bytes));
if (text !== 'thread A was here') {
  console.log(`FAIL threads-shared: the second Worker read ${JSON.stringify(text)} from the shared memory`);
  process.exit(1);
}

// The replay. A dispatch table belongs to a module instance, so this Worker
// began empty while the shared page directory already pointed at slots.
// Attaching must install every translated module into this Worker's tables, or
// a thread would dispatch into an unfilled slot.
const mods = await b.ask({ kind: 'modules' }, 'modules');
if (mods.n !== ran.modules) {
  console.log(
    `FAIL threads-shared: the joining Worker installed ${mods.n} of ${ran.modules} translated modules`,
  );
  process.exit(1);
}

// 3: and the kernel. The file exists only in the guest's filesystem, which is
// the shim's own data structure -- if the second Worker can read it, the two
// share a kernel rather than each having built one.
const file = await b.ask({ kind: 'file', path: '/hello' }, 'file');
if (file.text !== 'written by thread A') {
  console.log(
    `FAIL threads-shared: the second Worker read ${JSON.stringify(file.text)} from the shared kernel's filesystem`,
  );
  process.exit(1);
}

a.worker.terminate();
b.worker.terminate();
console.log(
  `ok   threads-shared (two Workers, one process memory, one kernel, ${ran.modules} modules replayed)`,
);
