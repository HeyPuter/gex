// Give a linked wasm module a second imported memory.
//
//   node tools/add-memory.mjs <in.wasm> <out.wasm> [module] [name] [pages]
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

// The guest's address space, in wasm pages: `GUEST_MEM_BYTES / WASM_PAGE`.
// Fixed rather than growable, because the guest's stack has to land where the
// ABI says it does.
const GUEST_PAGES = 0xffff0000 / 65536;

const [inPath, outPath, mod = 'env', name = 'process', pages = GUEST_PAGES] =
  process.argv.slice(2);
if (!inPath || !outPath) {
  console.error('usage: add-memory.mjs <in.wasm> <out.wasm> [module] [name] [pages]');
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

// The entry to append: kind 0x02 (memory), limits flag 0x03 -- a maximum, and
// shared.
//
// Shared, because the process memory *is* shared: it is one guest process's
// address space and every thread of that process is a Worker running on it.
// Only the sharing is narrower than the kernel's, which spans every process.
// So there is no "shared one and plain one" here; there are two shared
// memories with different scopes.
//
// That matters concretely rather than nominally: a memory import's `shared`
// flag is part of its *type*, and so are its minimum and maximum. A module
// that declares a plain memory will not accept a `SharedArrayBuffer` however
// large it is, and one that declares the wrong bounds will not link at all.
// Both numbers are the guest's fixed address space.
const entry = [
  ...str(mod), ...str(name),
  0x02, 0x03, ...enc(Number(pages)), ...enc(Number(pages)),
];

let at = 8; // past the magic and version
// Chunks, concatenated once at the end, rather than one big array pushed
// into. `out.push(...section)` spreads every byte as an argument, and a
// section of any real size overflows the call stack -- which it duly did as
// soon as this module grew a standard library.
const chunks = [src.subarray(0, 8)];
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
    const body = Uint8Array.from([
      ...enc(count + 1),
      ...src.subarray(after, end),
      ...entry,
    ]);
    chunks.push(Uint8Array.from([2, ...enc(body.length)]), body);
    found = true;
  } else {
    chunks.push(src.subarray(at, end));
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

const total = chunks.reduce((n, c) => n + c.length, 0);
const result = new Uint8Array(total);
{
  let at = 0;
  for (const c of chunks) {
    result.set(c, at);
    at += c.length;
  }
}
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
