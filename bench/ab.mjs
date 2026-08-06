// Compare two guest modules on the same program, in CPU time.
//
// Wall time is the wrong measurement on a shared machine. The box this was
// written on runs at a load average around twenty, and a minimum-of-N over
// interleaved wall-clock runs still swings 8-15% between rounds -- enough to
// have produced, in one sitting, a change that looked like +15% and -11% on
// the same benchmark. CPU time for the run phase alone does not care what
// else is on the machine, and it excludes the module compile and the bundle
// read, which are not what a codegen change is trying to move.
//
//   usage: node bench/ab.mjs [opts] <a.wasm> <b.wasm> <program> [args...]
//
//     --reps <n>        rounds of A then B (default 5)
//     --file <g>=<h>    bundle a file, as run-node.mjs does
//     --dir <g>=<h>     bundle a directory
//     --cwd <dir>       the guest's working directory
//     --stdin <path>    feed a file to the guest's standard input
//
// Each round runs A and B in fresh processes, so neither sees the other's
// warmed-up engine. The report is the minimum for each -- the least
// interfered-with round -- plus the spread across rounds, because a ratio is
// only worth reading next to how much the two sides moved on their own.

import { readdirSync, readFileSync, statSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const HERE = fileURLToPath(new URL('.', import.meta.url));

function parse(argv) {
  const o = { reps: 5, files: {}, cwd: null, stdin: null, rest: [] };
  let i = 0;
  for (; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--reps') o.reps = Number(argv[++i]);
    else if (a === '--cwd') o.cwd = argv[++i];
    else if (a === '--stdin') o.stdin = argv[++i];
    else if (a === '--file' || a === '--dir') {
      const spec = argv[++i] ?? '';
      const eq = spec.indexOf('=');
      if (eq <= 0) { console.error(`${a} wants <guest>=<host>`); process.exit(2); }
      const [g, h] = [spec.slice(0, eq), spec.slice(eq + 1)];
      if (a === '--file') o.files[g] = h;
      else {
        const walk = (gp, hp) => {
          for (const n of readdirSync(hp)) {
            const hc = `${hp}/${n}`, gc = `${gp}/${n}`;
            if (statSync(hc).isDirectory()) walk(gc, hc);
            else o.files[gc] = hc;
          }
        };
        walk(g.replace(/\/$/, ''), h.replace(/\/$/, ''));
      }
    } else break;
  }
  o.rest = argv.slice(i);
  return o;
}

const opt = parse(process.argv.slice(2));
const [wasmA, wasmB, prog, ...args] = opt.rest;
if (!wasmB || !prog) {
  console.error('usage: node bench/ab.mjs [opts] <a.wasm> <b.wasm> <program> [args...]');
  process.exit(2);
}

// The child measures itself: the parent cannot see where the run phase starts
// and stops, and process wall time would include node's own startup.
const CHILD = `${HERE}one.mjs`;

function once(wasm) {
  const r = spawnSync(process.execPath, [CHILD, wasm, prog, ...args], {
    input: opt.stdin ? readFileSync(opt.stdin) : undefined,
    env: {
      ...process.env,
      GEX_BENCH_FILES: JSON.stringify(opt.files),
      GEX_BENCH_CWD: opt.cwd ?? '',
    },
    maxBuffer: 1 << 28,
  });
  const line = String(r.stderr).split('\n').find((l) => l.startsWith('cpu-us '));
  if (!line) {
    console.error(String(r.stderr).slice(0, 800));
    throw new Error(`${wasm}: the run produced no measurement`);
  }
  return Number(line.slice(7));
}

const a = [];
const b = [];
for (let i = 0; i < opt.reps; i++) {
  a.push(once(wasmA));
  b.push(once(wasmB));
}
const ms = (us) => (us / 1000).toFixed(0);
const spread = (v) => (100 * (Math.max(...v) / Math.min(...v) - 1)).toFixed(0);
const [minA, minB] = [Math.min(...a), Math.min(...b)];
console.log(
  `A ${ms(minA)} ms (spread ${spread(a)}%)  B ${ms(minB)} ms (spread ${spread(b)}%)  ` +
    `B/A ${(minB / minA).toFixed(3)}`,
);
