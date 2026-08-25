// Is V8's own bounds check on a 64-bit memory cheaper than gex's explicit
// guard on a 32-bit one? That is the whole question behind moving gex to
// memory64: the guard is 7% of `crunch` (bench/WHAT-COSTS-WHAT.md) and a
// memory the engine bounds-checks itself would not need one.
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';

//   wat2wasm --enable-all bench/memory64/<name>.wat -o /tmp/<name>.wasm
//   node bench/memory64/probe.mjs <g32|m64|w32>
const HERE = fileURLToPath(new URL('.', import.meta.url));

const N = 40_000_000;
const which = process.argv[2];
const out = `/tmp/gex-m64-${which}.wasm`;
execFileSync('wat2wasm', ['--enable-all', `${HERE}${which}.wat`, '-o', out]);
const mod = new WebAssembly.Module(readFileSync(out));
const inst = new WebAssembly.Instance(mod, {});
const run = inst.exports.run;
run(1_000_000);                       // warm the tier-up
let best = Infinity;
for (let r = 0; r < 5; r++) {
  const t0 = process.cpuUsage();
  run(N);
  const t = process.cpuUsage(t0);
  best = Math.min(best, (t.user + t.system) / 1000);
}
console.log(`${which} ${best.toFixed(0)} ms`);
