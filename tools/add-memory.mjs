// Give a linked wasm module a second imported memory.
//
//   node tools/add-memory.mjs <in.wasm> <out.wasm> [module] [name]
//
// The global kernel module addresses its own memory as memory 0 -- that is
// what `--import-memory` gives it and what every Rust pointer in it means --
// and reaches a guest's buffers in a *second* memory with `memory.copy`.
// `gex-kmem` emits those instructions, but nothing in the Rust toolchain can
// declare the memory they name: there is no attribute for it, and wasm-ld
// emits exactly one memory. Without the declaration the module is not valid
// wasm and will not compile.
//
// So it is added here, after linking. This is a small edit and a safe one:
// memories have their own index space, so appending one cannot disturb a
// function, table or global index, and appending it *after* the existing
// import leaves the module's own memory at 0 where all its code expects it.
import { readFileSync, writeFileSync } from 'node:fs';

const [inPath, outPath, mod = 'env', name = 'process'] = process.argv.slice(2);
if (!inPath || !outPath) {
  console.error('usage: add-memory.mjs <in.wasm> <out.wasm> [module] [name]');
  process.exit(2);
}

const src = new Uint8Array(readFileSync(inPath));

/** LEB128 at `at`: the value, and where it ended. */
function uleb(bytes, at) {
  let value = 0, shift = 0, b;
  do {
    b = bytes[at++];
    value |= (b & 0x7f) << shift;
    shift += 7;
  } while (b & 0x80);
  return [value >>> 0, at];
}

/** LEB128, encoded. */
function enc(n) {
  const out = [];
  do {
    let b = n & 0x7f;
    n >>>= 7;
    if (n) b |= 0x80;
    out.push(b);
  } while (n);
  return out;
}

const str = (s) => [...enc(s.length), ...[...s].map((c) => c.charCodeAt(0))];

// The entry to append: kind 0x02 (memory), limits flag 0x00 (a minimum only).
//
// No maximum and not shared, deliberately. This memory is *someone else's* --
// a guest process's address space, which the host creates and sizes -- and an
// import whose declared type disagrees with the memory handed over will not
// link. A minimum of zero agrees with anything.
const entry = [...str(mod), ...str(name), 0x02, 0x00, ...enc(0)];

let at = 8; // past the magic and version
const out = [...src.subarray(0, 8)];
let found = false;

while (at < src.length) {
  const id = src[at];
  let size, bodyAt;
  [size, bodyAt] = uleb(src, at + 1);
  const end = bodyAt + size;
  if (id === 2) {
    // Rewrite the import section with one more entry. The count is a LEB and
    // may itself grow a byte, which is why the section length is recomputed
    // rather than adjusted.
    let count, after;
    [count, after] = uleb(src, bodyAt);
    const body = [...enc(count + 1), ...src.subarray(after, end), ...entry];
    out.push(2, ...enc(body.length), ...body);
    found = true;
  } else {
    out.push(...src.subarray(at, end));
  }
  at = end;
}

if (!found) {
  // A module that imports nothing at all has no section to extend. That is
  // not the kernel module -- it imports its own memory -- so this is a build
  // that went wrong rather than a case to handle.
  console.error(`add-memory: ${inPath} has no import section`);
  process.exit(1);
}

const result = new Uint8Array(out);
// Compile it here rather than discovering at run time that the surgery was
// wrong. This is the only check that the appended entry is well formed and
// that the `memory.copy` instructions referring to it now validate.
try {
  await WebAssembly.compile(result);
} catch (err) {
  console.error(`add-memory: the result does not compile: ${err.message}`);
  process.exit(1);
}

writeFileSync(outPath, result);
console.log(`  ${outPath}: ${result.length} bytes, imports ${mod}.${name} as memory 1`);
