// An x86 image mapped and never run is never read, hashed or placed; one
// that runs gets its unit at its first miss. Both DLLs (lazyplacedll.c,
// built twice) have a unit at their bases: lazyplace_a.dll is only mapped
// (DONT_RESOLVE_DLL_REFERENCES), lazyplace_b.dll is called (lazyplace.c).
// The first run finds where the DLLs are mapped and their units are built
// there. On the page's shim.
import { join } from 'node:path';
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1800 };

export default async function (t) {
  const w = wow64(t);
  const exe = await w.mingw('lazyplace');
  const dlls = {};
  for (const d of ['a', 'b'])
    dlls[d] = await w.mingw('lazyplacedll', { flags: ['-shared', '-nostdlib', '-e', '_DllMain@12'], libs: ['-lkernel32'], out: `lazyplace_${d}.dll` });
  const units = await w.providers(join(w.work, 'units'));
  const modes = [{ GEX_CGEN_PRECOMPILE: '1' }, { GEX_CGEN_PRECOMPILE: '1', GEX_CGEN_GUARD: '1' }];
  await w.unit(exe, units, { modes });
  const page = (name) => w.page(['/t/lazyplace.exe'], {
    files: { '/t/lazyplace.exe': exe, '/t/lazyplace_a.dll': dlls.a, '/t/lazyplace_b.dll': dlls.b },
    flags: ['--cgen-prefer'], env: { GEX_CGEN_CACHE: units }, name });
  const p0 = await page('p0');
  const bases = {};
  for (const d of ['a', 'b']) bases[d] = new RegExp(`^lazyplace_${d} at (?:0x)?([0-9a-fA-F]+)`, 'm').exec(p0.out)?.[1];
  if (!t.check('page: the DLLs loaded', bases.a && bases.b, w.detail(p0))) return w.clean();
  for (const d of ['a', 'b']) await w.unit(dlls[d], units, { base: `0x${bases[d]}`, modes });
  const p1 = await page('p1');
  t.check('page: the program finished', /^lazyplace ok /m.test(p1.out), w.detail(p1));
  t.check('page: lazyplace_b.dll, called, ran with its unit', /loaded .*\/lazyplace_b\.dll \(with its unit\)/.test(p1.out), w.detail(p1, 3));
  const a = /^.*loaded .*\/lazyplace_a\.dll.*$/m.exec(p1.out);
  t.check('page: lazyplace_a.dll, only mapped, was not placed', !a, a?.[0] ?? '');
  w.clean();
}
