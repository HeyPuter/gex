/**
 * Does the global kernel export everything the shim imports from it, with the
 * types the shim expects?
 *
 * The shim imports the process table, the filesystem, the pipes and the
 * shared module store from `gex_k`, which is a *separate wasm artefact* built
 * by `shims/kernel/build.sh`. For a long time nothing rebuilt it when the
 * shim was rebuilt, and nothing checked that the two agree.
 *
 * The failure that produced this is worth stating, because it looks like
 * nothing else: adding one kernel function and rebuilding only the shim gave
 * a run with no output at all. No error, no guest line, a renderer at half a
 * percent of one core. Instantiating the shim had failed on a missing import,
 * in a Worker, and whatever reported that was not being read. Seven hours of
 * staleness in a file nobody had thought about since.
 *
 * Names were not enough. Giving `k_obj_translated` a second parameter and
 * rebuilding only the shim passed this check -- every name was present -- and
 * then died in the browser with `imported function does not match the
 * expected type`, which is the same silent-wedge shape one level down. So the
 * signatures are compared too.
 *
 * The glue is checked against the shim already, by `check-glue.mjs`. The
 * three together mean a mismatch is a build error rather than a run that
 * produces nothing.
 */
import { readFile } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..', '..', '..');
const NS = 'gex_k';

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

function name(bytes, at) {
  const [len, from] = uleb(bytes, at);
  return [new TextDecoder().decode(bytes.subarray(from, from + len)), from + len];
}

const VAL = { 0x7f: 'i32', 0x7e: 'i64', 0x7d: 'f32', 0x7c: 'f64', 0x7b: 'v128' };
const val = (b) => VAL[b] || `?${b.toString(16)}`;

/**
 * The parts of a module this needs: its function types, which type each
 * function has, and its imports and exports.
 *
 * A function's index counts imported functions first, so an export naming
 * index `i` is an import when `i` is below the imported count and a defined
 * function otherwise. Getting that wrong reads every signature off by the
 * number of imports, which is a check that fails on correct builds.
 */
function parse(bytes) {
  let at = 8;
  const types = [];
  const importedFuncs = [];
  const definedFuncs = [];
  const imports = [];
  const exports_ = [];
  while (at < bytes.length) {
    const id = bytes[at++];
    let size;
    [size, at] = uleb(bytes, at);
    const end = at + size;
    if (id === 1) {
      let n;
      [n, at] = uleb(bytes, at);
      for (let i = 0; i < n; i++) {
        at++; // 0x60, a function type
        let np;
        [np, at] = uleb(bytes, at);
        const params = [];
        for (let p = 0; p < np; p++) params.push(val(bytes[at++]));
        let nr;
        [nr, at] = uleb(bytes, at);
        const results = [];
        for (let r = 0; r < nr; r++) results.push(val(bytes[at++]));
        types.push(`(${params.join(',')})->(${results.join(',')})`);
      }
    } else if (id === 2) {
      let n;
      [n, at] = uleb(bytes, at);
      for (let i = 0; i < n; i++) {
        let module_;
        let field;
        [module_, at] = name(bytes, at);
        [field, at] = name(bytes, at);
        const kind = bytes[at++];
        if (kind === 0) {
          let idx;
          [idx, at] = uleb(bytes, at);
          importedFuncs.push(idx);
          imports.push({ module: module_, field, type: idx });
        } else if (kind === 1) {
          at += 1;
          const lim = bytes[at++];
          [, at] = uleb(bytes, at);
          if (lim) [, at] = uleb(bytes, at);
        } else if (kind === 2) {
          const lim = bytes[at++];
          [, at] = uleb(bytes, at);
          if (lim) [, at] = uleb(bytes, at);
        } else {
          at += 2;
        }
      }
    } else if (id === 3) {
      let n;
      [n, at] = uleb(bytes, at);
      for (let i = 0; i < n; i++) {
        let idx;
        [idx, at] = uleb(bytes, at);
        definedFuncs.push(idx);
      }
    } else if (id === 7) {
      let n;
      [n, at] = uleb(bytes, at);
      for (let i = 0; i < n; i++) {
        let field;
        let idx;
        [field, at] = name(bytes, at);
        const kind = bytes[at++];
        [idx, at] = uleb(bytes, at);
        if (kind === 0) exports_.push({ field, index: idx });
      }
    }
    at = end;
  }
  const funcType = (i) =>
    i < importedFuncs.length
      ? types[importedFuncs[i]]
      : types[definedFuncs[i - importedFuncs.length]];
  return { types, imports, exports_, funcType };
}

const shimPath = process.argv[2];
const kernelPath = process.argv[3]
  || join(ROOT, 'web', 'public', 'gex', 'gex_kernel.plain.wasm');

const shim = parse(new Uint8Array(await readFile(shimPath)));
const kernel = parse(new Uint8Array(await readFile(kernelPath)));

const wanted = new Map();
for (const i of shim.imports) if (i.module === NS) wanted.set(i.field, shim.types[i.type]);
const has = new Map();
for (const e of kernel.exports_) has.set(e.field, kernel.funcType(e.index));

// `k_present` is the one the kernel does not supply, and cannot: it is the
// negotiation for whether there is a kernel at all, so the glue answers it --
// nought when it has no kernel to offer, one when it does. See `gex.mjs`.
const GLUE_ANSWERS = new Set(['k_present']);

const missing = [];
const wrong = [];
for (const [f, sig] of wanted) {
  if (GLUE_ANSWERS.has(f)) continue;
  if (!has.has(f)) missing.push(f);
  else if (has.get(f) !== sig) wrong.push(`${f}: shim wants ${sig}, kernel has ${has.get(f)}`);
}

if (missing.length || wrong.length) {
  if (missing.length) {
    console.error(
      `  the kernel is missing ${missing.length} of the ${wanted.size} functions `
      + `the shim imports from ${NS}: ${missing.sort().join(', ')}`,
    );
  }
  for (const w of wrong.sort()) console.error(`  ${w}`);
  console.error(
    '  it is a separate artefact -- rebuild it with shims/kernel/build.sh. '
    + 'Left stale, the shim fails to instantiate inside a Worker and the run '
    + 'produces no output at all.',
  );
  console.error(`kernel check FAILED: ${kernelPath}`);
  process.exit(1);
}
console.log(
  `  kernel supplies all ${wanted.size} functions the shim imports, with matching types`,
);
