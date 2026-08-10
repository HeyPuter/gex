/**
 * Does the global kernel export everything the shim imports from it?
 *
 * The shim imports the process table, the filesystem, the pipes and the
 * shared module store from `gex_k`, which is a *separate wasm artefact* built
 * by `shims/kernel/build.sh`. Nothing rebuilds it when the shim is rebuilt,
 * and nothing checked that the two agree.
 *
 * The failure that produced this is worth stating, because it looks like
 * nothing else: adding one kernel function and rebuilding only the shim gave
 * a run with no output at all. No error, no guest line, a renderer at half a
 * percent of one core. Instantiating the shim had failed on a missing import,
 * in a Worker, and whatever reported that was not being read. Seven hours of
 * staleness in a file nobody had thought about since.
 *
 * The glue is checked against the shim already, by `check-glue.mjs`. This is
 * the same idea for the other side, and the two together mean a mismatch is a
 * build error rather than a silent wedge.
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

/** Everything this module imports from `gex_k`. */
function kernelImports(bytes) {
  let at = 8;
  const wanted = new Set();
  while (at < bytes.length) {
    const id = bytes[at++];
    let size;
    [size, at] = uleb(bytes, at);
    const end = at + size;
    if (id === 2) {
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
          if (module_ === NS) wanted.add(field);
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
    }
    at = end;
  }
  return wanted;
}

/** Every function this module exports. */
function exports_(bytes) {
  let at = 8;
  const out = new Set();
  while (at < bytes.length) {
    const id = bytes[at++];
    let size;
    [size, at] = uleb(bytes, at);
    const end = at + size;
    if (id === 7) {
      let n;
      [n, at] = uleb(bytes, at);
      for (let i = 0; i < n; i++) {
        let field;
        [field, at] = name(bytes, at);
        const kind = bytes[at++];
        [, at] = uleb(bytes, at);
        if (kind === 0) out.add(field);
      }
    }
    at = end;
  }
  return out;
}

const shimPath = process.argv[2];
const kernelPath = process.argv[3]
  || join(ROOT, 'web', 'public', 'gex', 'gex_kernel.plain.wasm');

const wanted = kernelImports(new Uint8Array(await readFile(shimPath)));
const has = exports_(new Uint8Array(await readFile(kernelPath)));

// `k_present` is the one the kernel does not supply, and cannot: it is the
// negotiation for whether there is a kernel at all, so the glue answers it --
// nought when it has no kernel to offer, one when it does. See `gex.mjs`.
const GLUE_ANSWERS = new Set(['k_present']);

const missing = [...wanted]
  .filter((f) => !has.has(f) && !GLUE_ANSWERS.has(f))
  .sort();
if (missing.length) {
  console.error(
    `  the kernel is missing ${missing.length} of the ${wanted.size} functions `
    + `the shim imports from ${NS}: ${missing.join(', ')}`,
  );
  console.error(
    '  it is a separate artefact -- rebuild it with shims/kernel/build.sh. '
    + 'Left stale, the shim fails to instantiate inside a Worker and the run '
    + 'produces no output at all.',
  );
  console.error(`kernel check FAILED: ${kernelPath}`);
  process.exit(1);
}
console.log(`  kernel supplies all ${wanted.size} functions the shim imports`);
