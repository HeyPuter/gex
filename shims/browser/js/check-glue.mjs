// Does the glue match the shim it is going to load?
//
//   node shims/browser/js/check-glue.mjs [shim.wasm] [gex.mjs]
//
// Two wasm modules and a file of JavaScript between them, built by separate
// commands, and nothing makes them agree. Both directions have gone wrong:
//
//   * The shim gained an export the glue did not call, then the glue gained
//     the call and the published shim was a fortnight old. A visitor got
//     "is not a function" at the moment the guest first waited for input.
//   * The shim gained an *import*, `virgl_close`, and the glue did not
//     supply it. That one at least announces itself -- instantiation fails
//     outright -- but it announces itself after a browser has started, a
//     rootfs has been unpacked and a minute has gone by.
//
// The second is the dangerous shape, because the loud version is the lucky
// version. An import that keeps its name and gains a *parameter* is not an
// error to WebAssembly at all: JavaScript functions take any number of
// arguments, so the extra one arrives as `undefined`, the call succeeds, and
// what comes back is nonsense from a plausible-looking system. That is what
// makes this worth reading the binary for -- the import section carries the
// arity, and nothing else does.
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
 * each one is called with.
 *
 * Only the two sections that matter are decoded; the rest are skipped by
 * their length, which is what makes this short enough to keep honest.
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
          // Bit 0 is a maximum, bit 1 is `shared` -- both spellings appear,
          // because the shim is built twice.
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
 * What the glue supplies under `gex_env`, and with how many parameters.
 *
 * By source rather than by loading it, because loading it means building an
 * instance, and an instance is what this exists to check. Every entry in
 * that object is written `name: (args) => ...`, so that is what is matched;
 * an entry written any other way is reported rather than assumed fine.
 */
function suppliedArities(source) {
  // The object used to be written inline as `gex_env: { ... }`. It is a method
  // now, because a thread's Worker instantiates the same shim and needs the
  // same imports, so both `load` and `attach` ask for them. Either spelling is
  // accepted: what this reads is the object literal, wherever it is written.
  const marker = source.includes('#hostImports() {\n    return {')
    ? '#hostImports() {\n    return {'
    : `${NS}: {`;
  const at = source.indexOf(marker);
  if (at < 0) throw new Error(`no ${NS} import object in the glue`);
  // To the matching brace, counting depth, so a nested arrow body does not
  // end the object early.
  // Starting *at* the opening brace, not past it, so the depth count begins
  // at one and the loop below ends on the brace that closes this object.
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
  // `name: (a, b) => ...` and `name: () => ...`, at any indentation.
  for (const m of body.matchAll(/([A-Za-z_][A-Za-z0-9_]*)\s*:\s*\(([^)]*)\)\s*=>/g)) {
    const params = m[2].trim();
    out.set(m[1], params ? params.split(',').length : 0);
  }
  return out;
}

/** Everything the glue reaches for by name, less what it guards with `?.`. */
function calledExports(source) {
  const out = new Set();
  for (const m of source.matchAll(/exports\.(gex_[a-z0-9_]+)(\??)\.?/g)) {
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
  } else if (has.get(name) !== arity) {
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

// The globals the page *writes*, which the check above cannot see: it looks
// for calls, and `__stack_pointer` is assigned rather than called.
//
// It is not a nicety. Each Worker moves the shim's stack to its own slice
// before it runs anything, and a shim built without
// `--export=__stack_pointer` -- which is what a plain `cargo build` produces
// instead of `shims/browser/build.sh` -- fails that assignment with
// "Cannot set properties of undefined (setting 'value')" on the *first* boot,
// with nothing to connect it to the build. The demo site was broken this way
// by a stale artefact and the message named nothing.
for (const name of ['__stack_pointer']) {
  if (!exported.has(name)) {
    problems.push(
      `the shim has no export ${name}: it was built without `
      + '--export=__stack_pointer. Build it with shims/browser/build.sh, '
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
