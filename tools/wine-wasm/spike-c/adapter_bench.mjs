// The V8 cost of a winebuild-generated entry adapter (tests/wasm32/dll's
// abi.dll: gexp_abi_add, a stdcall int(int, int)) called the way a unit
// calls a cross-image function, against a unit calling a guest function of
// the same shape (spike-c's guest_to_guest).
//   node adapter_bench.mjs ABI_DLL_SO ADAPTER_CALLER_WASM [calls] [rounds]
import { readFileSync } from 'node:fs';
const [abiPath, callerPath] = process.argv.slice(2, 4);
const N = +(process.argv[4] || 20_000_000), ROUNDS = +(process.argv[5] || 5);
const memory = new WebAssembly.Memory({ initial: 1024, maximum: 65536, shared: true });
const table = new WebAssembly.Table({ initial: 64, element: 'anyfunc' });
const i32 = (v, mut = false) => new WebAssembly.Global({ value: 'i32', mutable: mut }, v);
// the module's data at 1 MiB, its table slots from 16, a C stack at 8 MiB
const abiMod = new WebAssembly.Module(readFileSync(abiPath));
const env = {
  memory, __indirect_function_table: table,
  __memory_base: i32(0x100000), __table_base: i32(16), __stack_pointer: i32(0x800000, true),
};
const imports = {};
for (const imp of WebAssembly.Module.imports(abiMod)) {
  imports[imp.module] ??= {};
  if (imp.module === 'env' && imp.name in env) imports.env[imp.name] = env[imp.name];
  else if (imp.kind === 'function') imports[imp.module][imp.name] = () => { throw new Error(`called ${imp.module}.${imp.name}`); };
}
imports.gex.provider_ctx = () => 0x1000;
const abi = new WebAssembly.Instance(abiMod, imports);
abi.exports.__wasm_apply_data_relocs?.();
// find gexp_abi_add's slot by name from the name section
const names = new Map();
{
  const b = readFileSync(abiPath); let p = 8;
  const leb = () => { let r = 0, s = 0, x; do { x = b[p++]; r |= (x & 0x7f) << s; s += 7; } while (x & 0x80); return r >>> 0; };
  const str = () => { const n = leb(); const t = b.toString('utf8', p, p + n); p += n; return t; };
  let elems = [];
  while (p < b.length) {
    const id = b[p++], len = leb(), end = p + len;
    if (id === 9) { leb(); /* count */ const flags = leb(); if (flags === 0) { while (b[p] !== 0x0b) p++; p++; const n = leb(); for (let i = 0; i < n; i++) elems.push(leb()); } }
    if (id === 0) { const nm = str(); if (nm === 'name') { while (p < end) { const sub = b[p++], sl = leb(), se = p + sl; if (sub === 1) { const n = leb(); for (let i = 0; i < n; i++) { const idx = leb(); names.set(idx, str()); } } p = se; } } }
    p = end;
  }
  globalThis.slotOf = (name) => 16 + elems.findIndex((f) => names.get(f) === name);
}
const slot = slotOf('gexp_abi_add');
const caller = new WebAssembly.Instance(new WebAssembly.Module(readFileSync(callerPath)), { env: { memory, __indirect_function_table: table } });
new BigUint64Array(memory.buffer, 0x1000 + 4 * 8, 1)[0] = 0x300000n;  // ESP
caller.exports.bench_call(1_000_000, slot);  // tier up
const res = [];
for (let r = 0; r < ROUNDS; r++) {
  const t0 = process.hrtime.bigint();
  caller.exports.bench_call(N, slot);
  res.push(Number(process.hrtime.bigint() - t0) / N);
}
res.sort((a, b) => a - b);
console.log(`node ${process.version}: x86 -> gexp_abi_add (winebuild adapter) median ${res[res.length >> 1].toFixed(2)} ns/call, min ${res[0].toFixed(2)}`);
