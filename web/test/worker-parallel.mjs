// Do N guests in N Workers actually run at the same time?
//
//   node web/test/worker-parallel.mjs <module.wasm> <elf> [args...]
//
// The whole reason for one Worker per guest process is parallelism: the old
// switcher ran every process on one wasm stack, so two never ran at once and
// `make -j4` in the guest was `make -j1` with bookkeeping. This measured
// whether the seam was worth building -- the same CPU-bound guest in 1, 2 and 4
// Workers, wall clock compared.
//
//   * near 1.0x scaling -- N Workers do N times the work; the design pays off.
//   * near 1/N -- something serializes, to be found before more Worker code.
//
// Each Worker loads its own module and runs its own guest, so this measures the
// runtime's ceiling, nothing shared or snapshotted.
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
 * The Worker times itself: what matters is how long a guest takes when N run,
 * not how long the parent waited (which includes serialized Worker startups).
 * Timed around `import`, so the number includes compiling the module, which
 * every Worker pays and which parallelizes the same way the guest does.
 */
function runOne() {
  return new Promise((resolve, reject) => {
    // `run-node.mjs` reads `process.argv`, so set it before importing: the
    // runner is a script, not a module with an entry point.
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
  // Perfect scaling: each of N guests takes as long as one did. Serialization
  // shows up as per-guest time, not the count.
  const scale = baseline / slowest;
  worst = Math.min(worst, scale);
  console.log(
    `  ${String(n).padStart(2)} worker${n === 1 ? ' ' : 's'}  ` +
      `wall ${secs.toFixed(2)}s  slowest guest ${slowest.toFixed(2)}s  ` +
      `${(n * scale).toFixed(2)}x throughput (perfect would be ${n}x)`,
  );
}

// A generous bar, on purpose: on a shared machine the point is to catch
// serialization, not grade the scheduler. Half is heavy contention; 1/N is
// serialized.
if (worst >= 0.5) {
  console.log(`ok   worker-parallel (${worst.toFixed(2)}x at the worst, nothing serializes)`);
  process.exit(0);
}
console.log(`FAIL worker-parallel: down to ${worst.toFixed(2)}x -- guests are not running at once`);
process.exit(1);
