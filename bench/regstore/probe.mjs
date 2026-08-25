// Is a guest register cheaper in a wasm global than in the context in memory?
//
// The question behind moving register traffic across block boundaries out of
// linear memory. Every block exit flushes the registers it wrote back to the
// context struct and the next block reloads them; a global that costs less
// than that store would take the difference back without duplicating a single
// block. This prices the four shapes against each other in isolation.
//
// Imported globals, not module-local ones: a closure is many instances and a
// register must be the same storage in all of them.
//
//   wat2wasm bench/regstore/<name>.wat && node probe.mjs <name>
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';

const HERE = fileURLToPath(new URL('.', import.meta.url));
const which = process.argv[2];
const out = `/tmp/gex-regstore-${which}.wasm`;
execFileSync('wat2wasm', ['--enable-tail-call', `${HERE}${which}.wat`, '-o', out]);
const memory = new WebAssembly.Memory({ initial: 16 });
const imports = { gex: { memory } };
for (const r of ['r0', 'r1', 'r2', 'r3']) {
  imports.gex[r] = new WebAssembly.Global({ value: 'i64', mutable: true }, 0n);
}
const inst = new WebAssembly.Instance(new WebAssembly.Module(readFileSync(out)), imports);
const run = inst.exports.run;
const N = which.endsWith('call') ? 8_000_000 : 30_000_000;
run(200_000);
let best = Infinity;
for (let r = 0; r < 5; r++) {
  const t0 = process.cpuUsage();
  run(N);
  const t = process.cpuUsage(t0);
  best = Math.min(best, (t.user + t.system) / 1000);
}
console.log(`${which} ${best.toFixed(0)} ms`);
