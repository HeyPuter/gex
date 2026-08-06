// Run one guest module once and report the CPU time its execution took.
//
// Driven by bench/ab.mjs, which sets the bundle and working directory in the
// environment. The measurement brackets `run` alone: loading the module and
// reading the bundle are real costs but not the ones a codegen change moves,
// and including them buries the signal.

import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { Gex, Env } from '../shims/browser/js/gex.mjs';

const [, , wasmPath, elfPath, ...args] = process.argv;

class Quiet extends Env {
  write() {}
  now() {
    return BigInt(Date.now()) * 1000000n;
  }
}

const files = {};
for (const [g, h] of Object.entries(JSON.parse(process.env.GEX_BENCH_FILES || '{}'))) {
  files[g] = readFileSync(h);
}
let stdin = null;
try {
  stdin = process.stdin.isTTY ? null : readFileSync(0);
} catch {
  stdin = null;
}

const shim = readFileSync(
  fileURLToPath(new URL('../target/wasm32-unknown-unknown/release/gex_browser.wasm', import.meta.url)),
);
const gex = new Gex(new Quiet());
const entry = await gex.load(shim, readFileSync(wasmPath), readFileSync(elfPath), files, [elfPath, ...args], {
  stdin,
  cwd: process.env.GEX_BENCH_CWD || null,
});

// The main thread only. `process.cpuUsage()` counts the whole process, and
// V8 compiles wasm on background threads -- so it charges the optimising
// compiler's work to the benchmark, and how much of that happens varies
// enough between runs to swing the number 20%. /proc gives per-thread times,
// and node's main thread is the one whose id is the process id.
const TICKS_PER_SEC = 100;
const selfStat = `/proc/self/task/${process.pid}/stat`;
function mainThreadCpuUs() {
  const f = readFileSync(selfStat, 'utf8');
  // The command name can contain spaces and parentheses; fields are counted
  // from after the last ')'.
  const rest = f.slice(f.lastIndexOf(')') + 2).split(' ');
  const utime = Number(rest[11]);
  const stime = Number(rest[12]);
  return ((utime + stime) / TICKS_PER_SEC) * 1e6;
}

const before = mainThreadCpuUs();
const status = await gex.run(entry);
const spent = mainThreadCpuUs() - before;
process.stderr.write(`cpu-us ${Math.round(spent)}\nstatus ${status}\n`);
