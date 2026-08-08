// Do N guests in N Workers actually run at the same time?
//
//   node web/test/worker-parallel.mjs <module.wasm> <elf> [args...]
//
// The whole reason for one Worker per guest process is parallelism. Today a
// page runs every guest process on one wasm stack and switches between them
// by snapshotting memory, so two processes never run at once no matter how
// many cores the machine has -- `make -j4` inside the guest is `make -j1`
// with bookkeeping.
//
// The transport that lets two Workers talk is built (`web/test/two-workers.mjs`),
// but transport is not the point; throughput is. So before building anything
// else on that seam, measure whether the seam is worth having: run the same
// CPU-bound guest in 1, 2 and 4 Workers and compare wall clock.
//
// What the numbers mean:
//
//   * **Scaling near 1.0x** -- N Workers finish N times the work in the time
//     one Worker takes one. The design pays off and the remaining work is
//     kernel state.
//   * **Scaling near 1/N** -- something serializes. That would be the thing
//     to find before writing another line of Worker code, because no amount
//     of shared-process-table work fixes it.
//
// This measures the *runtime*, not the switcher: each Worker loads its own
// module and runs its own guest, so nothing is shared and nothing is being
// snapshotted. It is the ceiling the design is aiming at, which is exactly
// what is worth knowing first.
import { Worker } from 'node:worker_threads';
import { readFile, access } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { cpus } from 'node:os';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..', '..');
const RUNNER = join(ROOT, 'shims/browser/js/run-node.mjs');

const [wasm, elf, ...rest] = process.argv.slice(2);
if (!wasm || !elf) {
  console.log('skip worker-parallel: give it a module and a program');
  process.exit(0);
}
for (const p of [wasm, elf, RUNNER]) {
  if (!(await access(p).then(() => true, () => false))) {
    console.log(`skip worker-parallel: missing ${p}`);
    process.exit(0);
  }
}

/**
 * One guest, in its own Worker, timed from inside.
 *
 * The Worker times itself rather than being timed from here, because what
 * matters is how long a guest takes when N of them are running -- not how
 * long the parent waited, which includes N Worker startups the parent
 * serialized by issuing them in a loop.
 *
 * Timed around `import` and not around the guest's own clock: the guest has
 * no clock unless the program prints one, and a program that prints nothing
 * is exactly the sort this should be able to measure. So the number includes
 * compiling the module, which every Worker pays and which parallelizes the
 * same way the guest does.
 */
function runOne() {
  return new Promise((resolve, reject) => {
    // `process.argv` is what `run-node.mjs` reads, so it is set before the
    // import rather than passed -- the runner is a script, not a module with
    // an entry point, and rewriting it to have one would change the thing
    // every other measurement in the tree was taken with.
    const w = new Worker(
      `
      import { parentPort, workerData } from 'node:worker_threads';
      process.argv = [process.argv[0], ${JSON.stringify(RUNNER)}, ...workerData.args];
      process.exit = () => {};   // or the runner ends the Worker before it reports
      const t0 = process.hrtime.bigint();
      await import(${JSON.stringify(RUNNER)});
      parentPort.postMessage(Number(process.hrtime.bigint() - t0) / 1e9);
      `,
      // Piped rather than inherited, so the guest's own output does not land
      // in the middle of the table.
      { eval: true, workerData: { args: [wasm, elf, ...rest] }, stdout: true, stderr: true },
    );
    w.stdout.resume();
    w.stderr.resume();
    w.on('message', (secs) => {
      w.terminate();
      resolve(secs);
    });
    w.on('error', reject);
  });
}

/** N guests at once: the wall clock, and how long each one took. */
async function round(n) {
  const t0 = process.hrtime.bigint();
  const each = await Promise.all(Array.from({ length: n }, () => runOne()));
  return { secs: Number(process.hrtime.bigint() - t0) / 1e9, each };
}

const CORES = cpus().length;
const COUNTS = [1, 2, 4].filter((n) => n <= CORES);
console.log(`worker-parallel: ${CORES} cores, ${elf.split('/').pop()}`);

let baseline = 0;
let worst = 1;
for (const n of COUNTS) {
  const { secs, each } = await round(n);
  const slowest = Math.max(...each);
  if (n === 1) baseline = slowest;
  // Perfect scaling means each of the N guests takes as long as the single
  // guest did: N times the work in the same wall clock. What degrades under
  // serialization is per-guest time, not the count.
  const scale = baseline / slowest;
  worst = Math.min(worst, scale);
  console.log(
    `  ${String(n).padStart(2)} worker${n === 1 ? ' ' : 's'}  ` +
      `wall ${secs.toFixed(2)}s  slowest guest ${slowest.toFixed(2)}s  ` +
      `${(n * scale).toFixed(2)}x throughput (perfect would be ${n}x)`,
  );
}

// A generous bar, on purpose: this runs on a shared machine, and the point is
// to catch serialization -- a lock, a shared table, a single-threaded
// translator -- not to grade the scheduler. Half is what a badly contended
// run looks like; 1/N is what a serialized one looks like.
if (worst >= 0.5) {
  console.log(`ok   worker-parallel (${worst.toFixed(2)}x at the worst, nothing serializes)`);
  process.exit(0);
}
console.log(`FAIL worker-parallel: down to ${worst.toFixed(2)}x -- guests are not running at once`);
process.exit(1);
