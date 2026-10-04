// The "AOT share" of a browser run (docs/noita-perf.md; the native host's
// GEX_NT_STATS line, shims/native/src/aot_share.rs), from V8 CPU profiles of
// the guest's Workers: each sample's own frame, classified by the module it
// is in, as V8 names it in `wasm://wasm/<module name>-<hash>`:
//
//   units      a unit (clang's link names its module `c.wasm...`)
//   providers  an export provider (`p.wasm...`)
//   runtime    the function table's runtime module (`gex-ftab-runtime`)
//   tier-1     any other wasm module (unnamed: `wasm://wasm/<hash>`)
//   host       the shim and the kernel (`gex_browser`, `gex_kernel`), and JS
//
// Idle, program and GC samples are left out. The share is of guest samples,
// AOT meaning units and providers.
//
//   node tests/nt/aot-share.mjs PROFILE.cpuprofile...

import { readFileSync } from 'node:fs';

const KINDS = ['units', 'providers', 'jit', 'tier-1', 'runtime'];

function kindOf(frame) {
  const url = frame.url ?? '';
  if (!url.startsWith('wasm://')) {
    return frame.functionName === '(idle)' || frame.functionName === '(program)' || frame.functionName === '(garbage collector)' || frame.functionName === '(root)'
      ? null : 'host';
  }
  const name = url.slice('wasm://wasm/'.length);
  if (name.startsWith('c.wasm')) return 'units';
  if (name.startsWith('p.wasm')) return 'providers';
  if (name.startsWith('gex-ftab-runtime')) return 'runtime';
  if (name.startsWith('gex_browser') || name.startsWith('gex_kernel')) return 'host';
  return 'tier-1';
}

/** Sample counts by kind (and `host`) over `profiles` (Chrome's shape). */
export function aotCounts(profiles) {
  const n = Object.fromEntries([...KINDS, 'host'].map((k) => [k, 0]));
  for (const p of profiles) {
    const kind = new Map(p.nodes.map((x) => [x.id, kindOf(x.callFrame)]));
    for (const s of p.samples ?? []) {
      const k = kind.get(s);
      if (k) n[k]++;
    }
  }
  return n;
}

/** The line, as the native host prints it. */
export function aotLine(n) {
  const guest = KINDS.reduce((a, k) => a + n[k], 0);
  const all = guest + n.host;
  const pct = (x, of) => (of ? (100 * x) / of : 0).toFixed(1);
  return `AOT ${pct(n.units + n.providers, guest)}% of guest (${KINDS.map((k) => `${k} ${pct(n[k], guest)}%`).join(' ')}); `
    + `host ${pct(n.host, all)}% of all; ${all} samples`;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const profiles = process.argv.slice(2).map((f) => JSON.parse(readFileSync(f, 'utf8')));
  console.log(aotLine(aotCounts(profiles)));
}
