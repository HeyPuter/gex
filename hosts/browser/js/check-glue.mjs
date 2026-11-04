// Does the glue match the shim it is going to load?
//
//   node hosts/browser/js/check-glue.mjs [shim.wasm] [gex.mjs]
//
// Two wasm modules and a file of JavaScript between them, built separately,
// and nothing else makes them agree. The dangerous case is silent: an import
// that keeps its name but gains a parameter is no error to WebAssembly -- JS
// functions take any number of arguments -- so the call succeeds with
// `undefined` and returns nonsense. Only the import section carries the arity,
// which is why this reads the binary.
import { readFile } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..', '..', '..');

/** The import module the shim declares everything under. */
const NS = 'gex_env';

/* -- reading the wasm ----------------------------------------------------- */

/** A LEB128 unsigned at `at`; returns the value and where it ended. */
function uleb(bytes, at) {
  let value = 0;
  let shift = 0;
  for (;;) {
    const b = bytes[at++];
    value |= (b & 0x7f) << shift;
    if (!(b & 0x80)) return [value >>> 0, at];
    shift += 7;
  }
}

/**
 * Every function this module imports from `gex_env`, and how many parameters
 * each takes. Only the type and import sections are decoded; the rest are
 * skipped by length.
 */
function importedArities(bytes) {
  let at = 8; // past the magic and the version
  const types = [];
  const wanted = new Map();
  while (at < bytes.length) {
    const id = bytes[at++];
    let size;
    [size, at] = uleb(bytes, at);
    const end = at + size;
    if (id === 1) {
      let n;
      [n, at] = uleb(bytes, at);
      for (let i = 0; i < n; i++) {
        at++; // 0x60, the one shape a type can have here
        let params;
        [params, at] = uleb(bytes, at);
        at += params;
        let results;
        [results, at] = uleb(bytes, at);
        at += results;
        types.push(params);
      }
    } else if (id === 2) {
      let n;
      [n, at] = uleb(bytes, at);
      const dec = new TextDecoder();
      for (let i = 0; i < n; i++) {
        let len;
        [len, at] = uleb(bytes, at);
        const mod = dec.decode(bytes.subarray(at, at + len));
        at += len;
        [len, at] = uleb(bytes, at);
        const name = dec.decode(bytes.subarray(at, at + len));
        at += len;
        const kind = bytes[at++];
        if (kind === 0x00) {
          let typeIdx;
          [typeIdx, at] = uleb(bytes, at);
          if (mod === NS) wanted.set(name, types[typeIdx]);
        } else if (kind === 0x01) {
          at++; // element type
          const limits = bytes[at++];
          [, at] = uleb(bytes, at);
          if (limits) [, at] = uleb(bytes, at);
        } else if (kind === 0x02) {
          const limits = bytes[at++];
          [, at] = uleb(bytes, at);
          // Bit 0 is a maximum, bit 1 is `shared` -- both appear (built twice).
          if (limits & 1) [, at] = uleb(bytes, at);
        } else {
          at += 2; // global: valtype, mutability
        }
      }
    }
    at = end;
  }
  return wanted;
}

/* -- reading the glue ----------------------------------------------------- */

/**
 * What the glue supplies under `gex_env`, and with how many parameters. Read
 * from source, not by loading it -- building an instance is what this checks.
 * Entries are matched as `name: (args) => ...`, or as `name: single.on ?
 * (args) => ... : wasm` (or `(SP_CODE && single.on) ? ...`; a wasm function in the threaded build: its arity is
 * the arrow's). An entry built by a glue method (`name: this.#make()`) is a
 * wasm function made at run time: supplied, with an arity the source cannot
 * show, so only its presence is checked.
 */
function suppliedArities(source) {
  // Either spelling: the inline `gex_env: { ... }` or the `#hostImports()`
  // method. What this reads is the object literal, wherever it is written.
  const marker = source.includes('#hostImports() {\n    return {')
    ? '#hostImports() {\n    return {'
    : `${NS}: {`;
  const at = source.indexOf(marker);
  if (at < 0) throw new Error(`no ${NS} import object in the glue`);
  // To the matching brace, counting depth so a nested arrow body does not end
  // the object early. Starts at the opening brace so depth begins at one.
  let depth = 0;
  let end = at + marker.length - 1;
  for (let i = end; i < source.length; i++) {
    if (source[i] === '{') depth++;
    else if (source[i] === '}' && --depth === 0) {
      end = i;
      break;
    }
  }
  const body = source.slice(at, end);
  const out = new Map();
  // `name: (a, b) => ...` and `name: () => ...`, at any indentation, and the
  // single-thread build's `name: single.on ? new WebAssembly.Suspending((a, b)
  // => ...) : (a, b) => ...`, read by its first arrow (both take the same).
  for (const m of body.matchAll(/([A-Za-z_][A-Za-z0-9_]*)\s*:\s*(?:(?:\(SP_CODE\s*&&\s*single\.on\)|single\.on)\s*\?\s*(?:new WebAssembly\.Suspending\(\s*)?)?\(([^)]*)\)\s*=>/g)) {
    const params = m[2].trim();
    out.set(m[1], params ? params.split(',').length : 0);
  }
  for (const m of body.matchAll(/([A-Za-z_][A-Za-z0-9_]*)\s*:\s*this\.#[A-Za-z_][A-Za-z0-9_]*\(\)\s*,/g)) {
    if (!out.has(m[1])) out.set(m[1], null);
  }
  return out;
}

/** Everything the glue reaches for by name, less what it guards with `?.`. */
function calledExports(source) {
  const out = new Set();
  // `e.gex_...` as well as `exports.gex_...`: the glue mostly aliases
  // `const e = this.#shim.exports`, so matching only the long form checked a
  // quarter of the calls. Optional calls (`e.gex_thing?.()`) are skipped --
  // that is how the glue tolerates an older shim without the export.
  for (const m of source.matchAll(/\b(?:e|exports)\.(gex_[a-z0-9_]+)(\??)/g)) {
    if (m[2] !== '?') out.add(m[1]);
  }
  return out;
}

/* -- the check ------------------------------------------------------------ */

const wasmPath = process.argv[2]
  || join(ROOT, 'target/wasm32-unknown-unknown/release/gex_browser.wasm');
const gluePath = process.argv[3] || join(HERE, 'gex.mjs');

const bytes = new Uint8Array(await readFile(wasmPath));
const glue = await readFile(gluePath, 'utf8');

const needs = importedArities(bytes);
const has = suppliedArities(glue);
const problems = [];

for (const [name, arity] of needs) {
  if (!has.has(name)) {
    problems.push(`the glue does not supply ${NS}.${name} -- the shim will not instantiate`);
  } else if (has.get(name) !== null && has.get(name) !== arity) {
    // The quiet one. Nothing throws; the call just gets the wrong arguments.
    problems.push(
      `${NS}.${name} takes ${arity} argument${arity === 1 ? '' : 's'} in the shim `
      + `and ${has.get(name)} in the glue`,
    );
  }
}

const mod = await WebAssembly.compile(bytes);
const exported = new Set(WebAssembly.Module.exports(mod).map((e) => e.name));
for (const name of calledExports(glue)) {
  if (!exported.has(name)) problems.push(`the shim has no export ${name}, and the glue calls it`);
}

// Globals the page writes, which the call check cannot see: `__stack_pointer`
// is assigned, not called. Each Worker moves the shim's stack to its own slice
// before running, and a shim built without `--export=__stack_pointer` (a plain
// `cargo build`, not `hosts/browser/build.sh`) fails that assignment on the
// first boot with nothing to connect it to the build.
for (const name of ['__stack_pointer']) {
  if (!exported.has(name)) {
    problems.push(
      `the shim has no export ${name}: it was built without `
      + '--export=__stack_pointer. Build it with hosts/browser/build.sh, '
      + 'not a plain cargo build.',
    );
  }
}

if (problems.length) {
  for (const p of problems) console.error(`  ${p}`);
  console.error(`glue check FAILED: ${wasmPath}`);
  process.exit(1);
}
console.log(
  `  glue matches the shim: ${needs.size} imports supplied, `
  + `${calledExports(glue).size} exports present`,
);
