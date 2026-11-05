// An AOT-only session (GEX_AOT_ONLY=1, as an ejected project runs: units
// only, nothing translated) faults as a translated one does (aotfault.c),
// natively (gexc run):
// - a call into a page that is not executable reaches the program's own
//   handler as an access violation, executing ("aotfault dep ok");
// - a call into code made at run time stops the run, naming it as code
//   outside every image;
// - a call through a window-procedure thunk made at run time, of either
//   documented shape, runs; one of another shape stops as above.
// A first, ordinary run of each case names the images it ran without a
// unit; those are built (minutes: Wine's own DLLs) and, with the exe's, are
// all an AOT-only run has.
import { join } from 'node:path';
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 3600 };

const GENERATED = /outside every image: code generated at run time/;

export default async function (t) {
  const w = wow64(t);
  const exe = await w.mingw('aotfault');
  const units = await w.providers(join(w.work, 'units'));
  await w.unit(exe, units, { modes: [{ GEX_CGEN_PRECOMPILE: '1' }] });
  const run = (arg, more, name) => w.native(['C:\\aotfault.exe', ...(arg ? [arg] : [])], { files: { 'aotfault.exe': exe },
    env: { GEX_CGEN_CACHE: units, GEX_WINE32_UNITS: 'all', ...more }, name });
  const verdict = (leg, r, want) => t.check(leg, want.test(r.out), w.detail(r));
  const missing = new Set();
  for (const c of ['hello', 'dep', 'gen', 'thunk']) {
    const r = await run(c === 'hello' ? '' : c, { GEX_WINE32_TRACE: '1' }, `warm-${c}`);
    if (c === 'dep') verdict('translated: dep', r, /^aotfault dep ok/m);
    for (const m of r.out.matchAll(/^gex-wine32: (\S+) at (0x[0-9a-f]+): no function-tier unit/gm)) missing.add(`${m[1]} ${m[2]}`);
  }
  // Every image the sessions ran without a unit, built at the base it had
  // (Wine's processes place the same DLL at the same base). One with no
  // code to build (a data-only DLL) needs none; a gap this leaves shows in
  // the AOT-only runs.
  t.say(`building ${missing.size} units`);
  for (const m of [...missing].sort()) {
    const [path, base] = m.split(' ');
    try { await w.unit(path, units, { base, modes: [{ GEX_CGEN_PRECOMPILE: '1' }] }); }
    catch (e) { t.say(`no unit for ${path} at ${base}: ${String(e.message).slice(0, 160)}`); }
  }
  const aot = { GEX_AOT_ONLY: '1' };
  verdict('aot-only: hello', await run('', aot, 'hello'), /^aotfault hello/m);
  verdict('aot-only: a non-executable page is the program\'s access violation', await run('dep', aot, 'dep'), /^aotfault dep ok/m);
  verdict('aot-only: code made at run time stops the run', await run('gen', aot, 'gen'), GENERATED);
  verdict('aot-only: window-procedure thunks made at run time run', await run('thunk', aot, 'thunk'), /^aotfault thunk ok/m);
  verdict('aot-only: a thunk of another shape stops the run', await run('near', aot, 'near'), GENERATED);
  w.clean();
}
