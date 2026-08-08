// A/B two statically recompiled modules under V8, honestly.
//
//   node bench/ab-browser.mjs <A.wasm> <B.wasm> <rootfs> <program> [args...]
//   node bench/ab-browser.mjs --calibrate <A.wasm> <rootfs> <program> [args...]
//
// The engine that matters is V8, so this runs the module the way a page
// does. What it mostly exists for is to stop me believing single-digit
// differences that are not there.
//
// # Why this file exists
//
// Two ad-hoc A/B loops in one session produced a 4.7% regression and a 6%
// improvement that both evaporated. Running the *same module against itself*
// explains why:
//
//     fannkuch  A 1.309s  B 1.084s  0.828x   individual runs 1.119-1.633
//     nbody     A 1.681s  B 1.599s  0.951x   individual runs 1.604-2.479
//     dicts     A 0.550s  B 0.532s  0.967x   individual runs 0.544-0.646
//
// Seventeen per cent apart on identical bytes, with individual runs spread
// by half. Anything smaller than about a fifth was noise, and I had been
// reporting fractions of that.
//
// Two causes, and this addresses both:
//
//   * **Order.** Those loops ran A then B in every round, so whatever warms
//     up -- the machine, the page cache, the CPU's clocks -- warmed up for
//     B. B came out ahead in all three benchmarks, which is the signature.
//     Rounds here alternate.
//   * **Tier-up.** The guest times itself, so translation is outside the
//     number, but V8's own tiering is not: a short benchmark measures
//     whichever mixture of Liftoff and TurboFan it happened to get. More
//     rounds and the minimum of them is the usual answer, and `--calibrate`
//     says whether it worked.
//
//   * **A warm page cache.** The first calibration handed the same *path*
//     to both sides, so the second read a cache the first had just filled
//     and won every benchmark -- a bias inside the measurement built to
//     detect bias. It copies the module now.
//
// `--calibrate` runs a module against a copy of itself. Every ratio it
// prints should be 1.000, and how far it is from 1.000 is the smallest
// difference this harness can see. Six rounds of the scaled benchmark, on
// an idle machine:
//
//     fannkuch  0.974x     nbody  1.059x     dicts  0.986x
//
// So about six per cent, and no side systematically ahead. Anything smaller
// than that is not a result. Re-run the calibration on the machine you are
// measuring on before believing a number close to it.
import { spawn } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..');
const RUNNER = join(ROOT, 'shims/browser/js/run-node.mjs');
const ROUNDS = Number(process.env.ROUNDS) || 8;

const argv = process.argv.slice(2);
const calibrate = argv[0] === '--calibrate';
if (calibrate) argv.shift();
// In calibration the two sides must be distinct *files*, not the same path
// twice. Handing over one path meant the second side always read a page
// cache the first had just warmed, and it came out ahead in every benchmark
// -- a bias in the very measurement built to detect bias.
let [a, b, rootfs, program, ...rest] = calibrate
  ? [argv[0], null, ...argv.slice(1)]
  : argv;
if (calibrate) {
  const { copyFileSync } = await import('node:fs');
  b = a.replace(/\.wasm$/, '') + '.calibrate-copy.wasm';
  copyFileSync(a, b);
}
if (!a || !b || !rootfs || !program) {
  console.error('usage: ab-browser.mjs [--calibrate] <A.wasm> [B.wasm] <rootfs> <program> [args...]');
  process.exit(2);
}

/** One run, returning every `name seconds` pair the guest printed. */
function once(wasm) {
  return new Promise((resolve) => {
    const p = spawn(
      process.execPath,
      [RUNNER, '--dir', `/=${rootfs}`, '--cwd', '/', '--argv0', program, wasm, join(rootfs, program), ...rest],
      { stdio: ['ignore', 'pipe', 'ignore'] },
    );
    let out = '';
    p.stdout.on('data', (d) => (out += d));
    p.on('close', () => {
      const got = [];
      for (const line of out.split('\n')) {
        const m = line.trim().match(/^(\S+)\s+([0-9.]+)/);
        if (m) got.push([m[1], Number(m[2])]);
      }
      resolve(got);
    });
  });
}

const times = { A: new Map(), B: new Map() };
const add = (side, pairs) => {
  for (const [name, secs] of pairs) {
    if (!times[side].has(name)) times[side].set(name, []);
    times[side].get(name).push(secs);
  }
};

for (let r = 0; r < ROUNDS; r++) {
  // Alternate, so nothing that warms up warms up for one side only.
  const order = r % 2 ? ['B', 'A'] : ['A', 'B'];
  for (const side of order) add(side, await once(side === 'A' ? a : b));
  process.stderr.write(`\rround ${r + 1}/${ROUNDS}`);
}
process.stderr.write('\n');

const min = (xs) => xs.reduce((m, x) => (x < m ? x : m), Infinity);
const spread = (xs) => (100 * (Math.max(...xs) - min(xs))) / min(xs);

console.log(calibrate ? 'CALIBRATION -- every ratio should be 1.000' : 'A/B');
for (const name of times.A.keys()) {
  const xa = times.A.get(name);
  const xb = times.B.get(name) || [];
  if (!xb.length) continue;
  console.log(
    `  ${name.padEnd(10)} A ${min(xa).toFixed(3)}s  B ${min(xb).toFixed(3)}s  ` +
      `${(min(xb) / min(xa)).toFixed(3)}x   spread A ${spread(xa).toFixed(0)}% B ${spread(xb).toFixed(0)}%`,
  );
}
