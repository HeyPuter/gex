// A guest that forks, with the child in a Worker of its own.
//
//   node web/test/fork-worker.mjs <static-aarch64-program-that-forks>
//
// This is the switcher's replacement, end to end. There is no snapshot and no
// switch: the parent keeps running in its Worker, and the child gets a Worker
// with a *copy* of the parent's mapped memory -- once, at `fork`, rather than
// twice on every switch for the life of both.
//
// What the host has to do is small, which is the point of having moved the
// shared state out first:
//
//   * make a memory and write the parent's regions into it,
//   * instantiate the shim and the guest module against it,
//   * `gex_adopt_child(pid, ctx, pc, sp, tls)` -- the child's own Worker
//     writes the few context words that differ, because the parent must not
//     touch a context it is still using,
//   * enter at what that returns.
//
// The process table is already shared, so the parent's `wait4` finds the
// child's status without either of them being switched to.
import { Worker } from 'node:worker_threads';
import { readFileSync, existsSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..', '..');
const SHIM = join(ROOT, 'target/wasm32-unknown-unknown/release/gex_browser.wasm');
const SEED = join(ROOT, 'web/public/gex/seed.wasm');
const GEX = join(ROOT, 'shims/browser/js/gex.mjs');
// The plain variant: these processes each have a memory of their own that is
// not a SharedArrayBuffer, and a memory import's `shared` flag is part of its
// type. The kernel's *own* memory is shared in both variants -- that is the
// one every Worker looks at.
const KERNEL = join(ROOT, 'web/public/gex/gex_kernel.plain.wasm');

const [elfPath] = process.argv.slice(2);
for (const p of [SHIM, SEED, elfPath]) {
  if (!p || !existsSync(p)) {
    console.log(`skip fork-worker: missing ${p ?? 'a program that forks'}`);
    process.exit(0);
  }
}

const shim = new Uint8Array(readFileSync(SHIM));
const seed = new Uint8Array(readFileSync(SEED));
const elf = new Uint8Array(readFileSync(elfPath));

// The global kernel: compiled once here and handed to every Worker, against
// one shared memory. That memory is where the process table lives, so the
// parent's `wait4` and the child's `exit_group` are talking about the same
// bytes even though neither can see the other's address space.
if (!existsSync(KERNEL)) {
  console.log('skip fork-worker: run shims/kernel/build.sh first');
  process.exit(0);
}
const kernelModule = await WebAssembly.compile(new Uint8Array(readFileSync(KERNEL)));
const kernelMemory = new WebAssembly.Memory({ initial: 2048, maximum: 16384, shared: true });
const kernel = { module: kernelModule, memory: kernelMemory };

// Every Worker's output, in arrival order, so a child's writes are visible
// even though it is not the process the harness started.
let output = '';
let forked = 0;

/**
 * One guest process in a Worker.
 *
 * `child` is null for the first one, which loads a program; otherwise it is
 * the record `fork` produced, and this Worker adopts it instead of loading.
 */
function run(child) {
  return new Promise((resolve, reject) => {
    const w = new Worker(
      `
      import { parentPort, workerData } from 'node:worker_threads';
      const { Gex, Env } = await import(${JSON.stringify(GEX)});
      class E extends Env {
        write(fd, bytes) {
          parentPort.postMessage({ kind: 'out', text: new TextDecoder().decode(bytes.slice()) });
        }
        now() { return BigInt(Date.now()) * 1000000n; }
        // The guest forked. Hand it up: only the parent process knows how to
        // make another Worker, and only it has this file.
        async spawnProcess(c) { parentPort.postMessage({ kind: 'fork', child: c }); }
      }
      const gex = new Gex(new E(), { kernel: workerData.kernel });
      if (process.env.GEX_STRACE) globalThis.__gexStrace = 1;
      if (process.env.GEX_DEBUG_EXIT) globalThis.GEX_DEBUG_EXIT = 1;
      const c = workerData.child;
      let entry;
      try {
      if (c) {
        // Everything a forked process is: the parent's memory, a shim and a
        // guest module over it, a kernel of its own, and the dispatch slots
        // the parent's code already branches into.
        entry = await gex.adoptProcess(workerData.shim, workerData.seed, c);
      } else {
        entry = await gex.load(workerData.shim, workerData.seed, workerData.elf,
                               {}, ['prog'], { strace: !!process.env.GEX_STRACE });
      }
      const status = await gex.run(entry);
      parentPort.postMessage({ kind: 'exit', status });
      } catch (err) {
        // Reported rather than left as an unhandled rejection: a child that
        // throws on the way in is silent otherwise, and silence here reads
        // exactly like a child that is running and slow.
        parentPort.postMessage({ kind: 'threw', message: err?.stack ?? String(err) });
      }
      `,
      { eval: true, workerData: { shim, seed, elf, child, kernel } },
    );
    w.on('message', (m) => {
      if (m.kind === 'out') {
        output += m.text;
        // Printed as it arrives rather than at the end: a fork that half
        // works hangs, and a harness that only reports on the way out says
        // nothing at all about the half that did work.
        process.stdout.write(`[${child ? `pid ${child.pid}` : 'parent'}] ${m.text}`);
      } else if (m.kind === 'fork') {
        console.log(`[fork] pid ${m.child.pid}: ${m.child.image.length} regions, ` +
          `${(m.child.pages?.byteLength ?? 0) / 12} pages, ${m.child.modules?.length ?? 0} modules`);
        forked++;
        // The child is a process in its own right: start it and let it run
        // alongside the parent, which is the whole difference from a switch.
        run(m.child).catch(reject);
      } else if (m.kind === 'threw') {
        console.log(`[threw] ${child ? `pid ${child.pid}` : 'parent'}: ${m.message}`);
        w.terminate();
        reject(new Error(m.message));
      } else if (m.kind === 'exit') {
        console.log(`[exit] ${child ? `pid ${child.pid}` : 'parent'}: status ${m.status}`);
        w.terminate();
        resolve(m.status);
      }
    });
    w.on('error', (err) => {
      console.log(`[error] ${child ? `pid ${child.pid}` : 'parent'}: ${err.message}`);
      reject(err);
    });
  });
}

const status = await run(null);

if (!forked) {
  console.log('FAIL fork-worker: the guest never forked, so nothing was tested');
  process.exit(1);
}
if (!output.includes('parent') || !output.includes('child')) {
  console.log('FAIL fork-worker: only one side of the fork produced output');
  console.log(`     forked ${forked} time(s), exit ${status}`);
  console.log(`     output: ${JSON.stringify(output)}`);
  process.exit(1);
}
console.log(`ok   fork-worker (${forked} child in its own Worker, no snapshot taken)`);
