// How long V8 takes to compile a module, which is what a page waits for.
//
//   node bench/compile.mjs <a.wasm> <b.wasm> [reps]
//
// Not the same question as `bench/ab.mjs`, and not answered by it: that one
// deliberately excludes the compile so a codegen change is not measured
// through it. But a browser pays this once per module per load, a closure is
// tens of megabytes of them, and a change that makes the module smaller is
// worth something here even when it is worth nothing at run time.
//
// Each module is compiled in a fresh process, because V8 caches compiled
// modules within one.
import { readFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const self = fileURLToPath(import.meta.url);

if (process.env.GEX_COMPILE_ONE) {
  const bytes = readFileSync(process.env.GEX_COMPILE_ONE);
  const t0 = process.hrtime.bigint();
  await WebAssembly.compile(bytes);
  const t1 = process.hrtime.bigint();
  process.stdout.write(String(Number(t1 - t0) / 1e6));
  process.exit(0);
}

const [a, b, reps = '7'] = process.argv.slice(2);
if (!a || !b) {
  console.error('usage: node bench/compile.mjs <a.wasm> <b.wasm> [reps]');
  process.exit(2);
}

function best(path) {
  let ms = Infinity;
  for (let i = 0; i < Number(reps); i++) {
    const r = spawnSync(process.execPath, [self], {
      env: { ...process.env, GEX_COMPILE_ONE: path },
      encoding: 'utf8',
    });
    const v = Number(r.stdout);
    if (Number.isFinite(v)) ms = Math.min(ms, v);
  }
  return ms;
}

// Interleaved, so a machine that gets busier partway does not land on one side.
let ba = Infinity, bb = Infinity;
for (let i = 0; i < 2; i++) {
  ba = Math.min(ba, best(a));
  bb = Math.min(bb, best(b));
}
const sa = readFileSync(a).length, sb = readFileSync(b).length;
console.log(
  `A ${ba.toFixed(0)} ms (${(sa / 1048576).toFixed(2)} MiB)  ` +
  `B ${bb.toFixed(0)} ms (${(sb / 1048576).toFixed(2)} MiB)  B/A ${(bb / ba).toFixed(3)}`,
);
