// Codegen benchmark harness (docs/gexcc.md §7.3): load one variant's modules
// with a small dynamic linker (dylink.0, GOT.mem/GOT.func, cross-module
// function imports), run each kernel, print the timings as JSON.
//   node harness.mjs VARIANT   (out/VARIANT.wasm, its side modules)
import { readFileSync } from 'node:fs';
const dir = new URL('./out/', import.meta.url);
const variant = process.argv[2];
const sets = {
  static: [['static.wasm', 'static']], pie: [['pie.wasm']], 'pie-dv': [['pie-dv.wasm']], 'pie-opt': [['pie-opt.wasm']],
  'asyncify-all': [['asyncify-all.wasm']], 'asyncify-ind': [['asyncify-ind.wasm']], lp64: [['lp64.wasm', 'static']],
  split: [['libc.so.wasm'], ['split.wasm']], lib: [['libk.so.wasm'], ['lib.wasm']],
  'static-opt': [['static-opt.wasm', 'static']], 'lp64-noopt': [['lp64-noopt.wasm', 'static']],
  size64: [['size64.wasm', 'static']], w64: [['w64.wasm', 'static']],
};
const entry = variant === 'lib' ? 'run' : 'bench_run';

function leb(b, p) { let r = 0, s = 0, x; do { x = b[p++]; r |= (x & 0x7f) << s; s += 7; } while (x & 0x80); return [r >>> 0, p]; }
function dylink(bytes) {
  let p = 8;
  while (p < bytes.length) {
    const id = bytes[p]; let n; [n, p] = leb(bytes, p + 1); const end = p + n;
    if (id === 0) {
      let l; [l, p] = leb(bytes, p); const name = new TextDecoder().decode(bytes.subarray(p, p + l)); p += l;
      if (name === 'dylink.0') {
        while (p < end) {
          const t = bytes[p]; let sl; [sl, p] = leb(bytes, p + 1);
          if (t === 1) { let a, b, c, d, q = p; [a, q] = leb(bytes, q); [b, q] = leb(bytes, q); [c, q] = leb(bytes, q); [d, q] = leb(bytes, q); return { memSize: a, memAlign: b, tableSize: c, tableAlign: d }; }
          p += sl;
        }
      }
    }
    p = end;
  }
  return null;
}

// w64: wasm64 unlowered, on a 64-bit memory (V8's memory64)
const memory = variant === 'w64'
  ? new WebAssembly.Memory({ initial: 1024n, maximum: 65536n, shared: true, address: 'i64' })
  : new WebAssembly.Memory({ initial: 1024, maximum: 65536, shared: true });
const table = new WebAssembly.Table({ initial: 16, element: 'anyfunc' });
const sp = new WebAssembly.Global({ value: 'i32', mutable: true }, 0x200000); // stack below 2 MiB
let memNext = 0x400000;
const insts = [];
const got = new Map(); // name -> {g, kind}
const tCompile0 = performance.now();
const mods = sets[variant].map(([f, kind]) => {
  const bytes = readFileSync(new URL(f, dir));
  return { f, kind, bytes, mod: new WebAssembly.Module(bytes), dl: kind === 'static' ? null : dylink(bytes) };
});
const compileMs = performance.now() - tCompile0;
const byName = (name) => { for (const i of insts) { const e = i.inst.exports[name]; if (e !== undefined) return { i, e }; } return null; };
for (const m of mods) {
  let memBase = 0, tableBase = 0;
  if (m.dl) {
    const al = 1 << m.dl.memAlign;
    memBase = (memNext + al - 1) & ~(al - 1); memNext = memBase + m.dl.memSize;
    tableBase = table.length; table.grow(m.dl.tableSize);
  }
  const env = { memory, __indirect_function_table: table, __stack_pointer: sp,
    __memory_base: new WebAssembly.Global({ value: 'i32' }, memBase), __table_base: new WebAssembly.Global({ value: 'i32' }, tableBase) };
  const GOTmem = {}, GOTfunc = {};
  for (const im of WebAssembly.Module.imports(m.mod)) {
    if (im.module === 'GOT.mem' || im.module === 'GOT.func') {
      const g = new WebAssembly.Global({ value: 'i32', mutable: true }, 0);
      (im.module === 'GOT.mem' ? GOTmem : GOTfunc)[im.name] = g;
      got.set(im.module + ':' + im.name, g);
    } else if (im.module === 'env' && im.kind === 'function') {
      const d = byName(im.name);
      env[im.name] = d ? d.e : (...a) => { throw new Error(`unresolved ${im.name}`); };
    }
  }
  const inst = new WebAssembly.Instance(m.mod, { env, 'GOT.mem': GOTmem, 'GOT.func': GOTfunc });
  insts.push({ inst, memBase, f: m.f });
}
// GOT: data symbols are exported as offsets from their module's memory base
for (const [k, g] of got) {
  const [mod, name] = k.split(':');
  const d = byName(name);
  if (!d) continue;
  if (mod === 'GOT.mem') g.value = d.i.memBase + (d.e.value >>> 0);
  else { const idx = table.length; table.grow(1); table.set(idx, d.e); g.value = idx; }
}
for (const { inst } of insts) { inst.exports.__wasm_apply_data_relocs?.(); }
for (const { inst } of insts) { inst.exports.__wasm_call_ctors?.(); }
const run = byName(entry).e;
const names = ['zlib', 'zstd', 'trees', 'strings'];
const res = { variant, compileMs: +compileMs.toFixed(1), bytes: mods.reduce((a, m) => a + m.bytes.length, 0) };
for (let k = 0; k < 4; k++) {
  run(k, 1); // warm-up (tier-up)
  const t = [];
  let sum = 0;
  for (let r = 0; r < 5; r++) { const t0 = performance.now(); sum = run(k, 1); t.push(performance.now() - t0); }
  t.sort((a, b) => a - b);
  res[names[k]] = +t[2].toFixed(2);
  res[names[k] + '_sum'] = sum >>> 0;
}
console.log(JSON.stringify(res));
