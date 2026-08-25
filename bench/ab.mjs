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
// warmed-up engine. The headline is the *median of the per-round ratios*, not
// the ratio of the two minima. The two are not the same thing and the
// difference matters: on a benchmark with a large working set, CPU time
// tracks how much cache the rest of the machine is using, which drifts over
// minutes. Comparing A and B from the same round cancels that drift; comparing
// two minima drawn from different rounds does not.
//
// How much it matters was measured by running this against two identical
// builds. On a cache-resident benchmark the two agree and both read 1.00. On
// one with a 160 KB working set the minima read 0.94 -- a 6% win, from no
// change at all -- while the paired median read 1.00. Anything smaller than
// the null-test figure below is not a result.

import { readdirSync, readFileSync, statSync, unlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
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
        // `stat`, and tolerate what it cannot answer. A real distribution rootfs
        // has dangling symlinks in it -- Debian ships /etc/rmt pointing at a
        // package that is not installed -- and `stat` on one throws, which
        // took down the whole harness before any benchmark ran. A link is
        // bundled as whatever it resolves to, or skipped if that is nothing.
        const walk = (gp, hp) => {
          for (const n of readdirSync(hp)) {
            const hc = `${hp}/${n}`, gc = `${gp}/${n}`;
            let st;
            try { st = statSync(hc); } catch { continue; }
            if (st.isDirectory()) walk(gc, hc);
            else if (st.isFile()) o.files[gc] = hc;
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

// Through a file, not the environment. `--dir` over a whole distribution
// rootfs is tens of thousands of entries and megabytes of JSON, and an
// environment that big fails `exec` with E2BIG -- which arrives as a child
// that produced no measurement rather than as an error that says so.
const MAP = `${tmpdir()}/gex-ab-files-${process.pid}.json`;
writeFileSync(MAP, JSON.stringify(opt.files));
process.on('exit', () => { try { unlinkSync(MAP); } catch {} });

function once(wasm) {
  const r = spawnSync(process.execPath, [CHILD, wasm, prog, ...args], {
    input: opt.stdin ? readFileSync(opt.stdin) : undefined,
    env: {
      ...process.env,
      GEX_BENCH_FILES_JSON: MAP,
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

// A then B on even rounds, B then A on odd ones. Always running A first
// gives B a warm page cache and a clock that has already boosted, and that is
// worth about 2.5% here: two identical builds compared in a fixed order read
// as a consistent 2.5% win for whichever went second. Alternating cancels it.
const a = [];
const b = [];
for (let i = 0; i < opt.reps; i++) {
  if (i % 2 === 0) {
    a.push(once(wasmA));
    b.push(once(wasmB));
  } else {
    b.push(once(wasmB));
    a.push(once(wasmA));
  }
}
const ms = (us) => (us / 1000).toFixed(0);
const spread = (v) => (100 * (Math.max(...v) / Math.min(...v) - 1)).toFixed(0);
const median = (v) => {
  const s = [...v].sort((x, y) => x - y);
  const h = s.length >> 1;
  return s.length % 2 ? s[h] : (s[h - 1] + s[h]) / 2;
};
const ratios = a.map((x, i) => b[i] / x);
// How far the per-round ratios scatter, as the median distance from the
// median. The full range would be the obvious measure and is the wrong one
// with a handful of rounds: one round where something else woke up on the
// machine sets it, and it then reports the worst round rather than the usual
// one. A result closer to 1.0 than this figure is indistinguishable from
// having changed nothing.
const mid = median(ratios);
const noise = (100 * median(ratios.map((r) => Math.abs(r - mid)))).toFixed(1);
console.log(
  `A ${ms(median(a))} ms (spread ${spread(a)}%)  B ${ms(median(b))} ms (spread ${spread(b)}%)  ` +
    `B/A ${mid.toFixed(3)} (+/-${noise}%)`,
);
