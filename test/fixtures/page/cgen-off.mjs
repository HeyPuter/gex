// The page's `cgen=0` switches every unit off, the ELF objects' as well: no
// cgen mode reaches the runtime and no object is named for a unit, whatever
// else the build's switches list.
//
//   node test/fixtures/page/cgen-off.mjs
import { pathToFileURL as gexPathToFileURL } from 'node:url';
import { join as gexJoin } from 'node:path';
// gex: the checkout these fixtures are in, or GEX_ROOT.
const GEX_ROOT = process.env.GEX_ROOT || gexJoin(import.meta.dirname, '..', '..', '..');
const gexUrl = (p) => gexPathToFileURL(gexJoin(GEX_ROOT, p)).href;
// The page's switches onto the runtime's globals: what app.js `start()`
// calls, in a module of its own (no page needed). An older checkout had it
// in app.js, which imports the runtime as `@gex/...`.
import { existsSync } from 'node:fs';
import { register } from 'node:module';
const js = gexUrl('hosts/browser/js/');
register(`data:text/javascript,${encodeURIComponent(`export async function resolve(s, c, next) {
  return next(s.startsWith('@gex/') ? ${JSON.stringify(js)} + s.slice(5) : s, c);
}`)}`);
const mod = existsSync(gexJoin(GEX_ROOT, 'page/src/page-globals.js'))
  ? await import(gexUrl('page/src/page-globals.js'))
  : await import(gexUrl('page/src/app.js'));
// Newer: the settings are page-config.js's `page`, not globals.
const { page } = existsSync(gexJoin(GEX_ROOT, 'page/src/page-config.js'))
  ? await import(gexUrl('page/src/page-config.js'))
  : { page: null };
const setGlobals = mod.setPageConfig ?? mod.setGlobals;

let fail = 0;
function run(q) {
  for (const k of ['GEX_CGEN', 'GEX_ELF_UNITS']) delete globalThis[k];
  const m = new Map(Object.entries(q));
  setGlobals((k) => m.get(k) ?? null, { arch: 'aarch64' });
  return page ? { cgen: page.cgen, elf: page.elfUnits } : { cgen: globalThis.GEX_CGEN, elf: globalThis.GEX_ELF_UNITS };
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
