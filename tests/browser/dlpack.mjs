#!/usr/bin/env node
// A wasm32 library's exports packed (gex.mjs `packExports`): the function
// exports the page does not call by name go, the table slots the loader
// filled with them (`X`) are filled by added element segments, and a
// module of another shape is left alone. Needs wat2wasm.
//
//   node tests/browser/dlpack.mjs

import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { packExports } from '../../shims/browser/js/gex.mjs';

const root = mkdtempSync(join(tmpdir(), 'gex-dlpack-'));
let failed = 0;
const check = (what, got, want) => {
  const ok = JSON.stringify(got) === JSON.stringify(want);
  if (!ok) failed++;
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${what}${ok ? '' : `\n  got  ${JSON.stringify(got)}\n  want ${JSON.stringify(want)}`}`);
};
const wasm = (wat) => {
  writeFileSync(join(root, 'm.wat'), wat);
  execFileSync('wat2wasm', ['--enable-all', join(root, 'm.wat'), '-o', join(root, 'm.wasm')]);
  return new Uint8Array(readFileSync(join(root, 'm.wasm')));
};
const lib = (elem, more = '') => wasm(`(module
  (import "env" "__indirect_function_table" (table 0 funcref))
  (import "env" "__table_base" (global $tb i32))
  (import "env" "imp" (func $imp (result i32)))
  ${more}
  (global $d i32 (i32.const 1234))
  (global $m (mut i32) (i32.const 5))
  (func $a (result i32) i32.const 1)
  (func $b (result i32) i32.const 2)
  (func $c (result i32) i32.const 3)
  (func $e (result i32) i32.const 4)
  ${elem ? '(elem (global.get $tb) $a)' : ''}
  (export "a" (func $a)) (export "b" (func $b)) (export "c" (func $c)) (export "e" (func $e))
  (export "__wasm_call_ctors" (func $a)) (export "main" (func $b))
  (export "data_sym" (global $d)) (export "mut_sym" (global $m)) (export "__tls_size" (global $d)))`);
try {
  const base = 10;
  const extra = [[base + 5, 'e'], [base + 1, 'b'], [base + 2, 'c']];
  for (const elem of [true, false]) {
    const p = packExports(lib(elem), extra, base);
    const mod = new WebAssembly.Module(p);
    check(`${elem ? 'with' : 'without'} an element section: exports kept`,
      WebAssembly.Module.exports(mod).map((x) => x.name).sort(), ['__tls_size', '__wasm_call_ctors', 'main', 'mut_sym']);
    const table = new WebAssembly.Table({ element: 'anyfunc', initial: 20 });
    new WebAssembly.Instance(mod, { env: {
      __indirect_function_table: table, imp: () => 0,
      __table_base: new WebAssembly.Global({ value: 'i32', mutable: false }, base),
    } });
    check(`${elem ? 'with' : 'without'} an element section: table`,
      [10, 11, 12, 13, 14, 15].map((i) => table.get(i)?.() ?? null), elem ? [1, 2, 3, null, null, 4] : [null, 2, 3, null, null, 4]);
  }
  check('fpcast thunks: not packed', packExports(lib(true, '(func $t (export "__fpcast$x"))'), extra, base), null);
  check('two tables: not packed', packExports(lib(true, '(table 1 funcref)'), extra, base), null);
  check('an X export it does not have: not packed', packExports(lib(true), [[base + 1, 'nope']], base), null);
} finally {
  rmSync(root, { recursive: true, force: true });
}
process.exit(failed ? 1 : 0);
