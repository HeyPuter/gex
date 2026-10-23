// The page's `cgen=0` switches every unit off, the ELF objects' as well: no
// cgen mode reaches the runtime and no object is named for a unit, whatever
// else the build's switches list.
//
//   node tests/page-switches/cgen-off.mjs
import { register } from 'node:module';

// app.js imports the runtime as `@gex/...`, the page build's alias
// (web/vite.config.js) for shims/browser/js
const js = new URL('../../shims/browser/js/', import.meta.url).href;
register(`data:text/javascript,${encodeURIComponent(`export async function resolve(s, c, next) {
  return next(s.startsWith('@gex/') ? ${JSON.stringify(js)} + s.slice(5) : s, c);
}`)}`);
const { setGlobals } = await import('../../web/src/app.js');

let fail = 0;
function run(q) {
  for (const k of ['GEX_CGEN', 'GEX_ELF_UNITS']) delete globalThis[k];
  const m = new Map(Object.entries(q));
  setGlobals((k) => m.get(k) ?? null, { arch: 'aarch64' });
  return { cgen: globalThis.GEX_CGEN, elf: globalThis.GEX_ELF_UNITS };
}
function expect(name, q, cgenOn) {
  const r = run(q);
  const ok = cgenOn ? r.cgen > 0 && r.elf === q.elfunits : r.cgen === undefined && r.elf === undefined;
  console.log(`${ok ? 'ok' : 'FAIL'} ${name}: GEX_CGEN=${r.cgen} GEX_ELF_UNITS=${r.elf}`);
  if (!ok) fail = 1;
}
expect('cgen=0 with ELF units listed', { cgen: '0', elfunits: 'libQt6Core.so.6,liba.so' }, false);
expect('cgen=1 with ELF units listed', { cgen: '1', elfunits: 'libQt6Core.so.6,liba.so' }, true);
process.exit(fail);
