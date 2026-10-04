// W0(c): run boundary.wasm's benchmarks in V8 and print ns per call.
//   node bench.mjs [boundary.wasm] [calls] [rounds]
import { readFileSync } from 'node:fs';
const file = process.argv[2] || new URL('boundary.wasm', import.meta.url).pathname;
const N = +(process.argv[3] || 20_000_000), ROUNDS = +(process.argv[4] || 7);
const memory = new WebAssembly.Memory({ initial: 1024, maximum: 1024, shared: true });
const { instance } = await WebAssembly.instantiate(readFileSync(file), { env: { memory } });
const x = instance.exports;
x.setup();
const names = (process.env.BENCH || 'native_direct,native_indirect,native_icall,native_icall2,native_to_guest,native_to_guest2,guest_to_guest,guest_to_native').split(',');
for (const n of names) x['bench_' + n](1_000_000); // tier up (TurboFan)
const res = {};
for (let r = 0; r < ROUNDS; r++) {
  for (const n of names) {
    const t0 = process.hrtime.bigint();
    x['bench_' + n](N);
    const dt = Number(process.hrtime.bigint() - t0) / N;
    (res[n] ??= []).push(dt);
  }
}
const med = a => a.slice().sort((p, q) => p - q)[a.length >> 1];
console.log(`node ${process.version}, ${N} calls x ${ROUNDS} rounds, median ns/call`);
for (const n of names) console.log(n.padEnd(18), med(res[n]).toFixed(2), '  min', Math.min(...res[n]).toFixed(2));
