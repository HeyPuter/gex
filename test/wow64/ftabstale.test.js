// A thread that runs on across another thread's unit install (ftabstale.c):
// unit code calls a function pointer through the process-wide probe cache
// straight into its own Worker's function table, so an index another Worker
// published must be in this Worker's table, not only after it next turns
// out to JS ("table index is out of bounds"). The table's room is cut to 8
// (GEX_TEST_FTAB_SPARE) so ftabdll's unit outruns it. The exe's unit is
// built at its fixed base; the first run finds where ftabdll is placed and
// its unit is built there. On the page's shim, multi-Worker only: the
// single-thread build has one table.
import { join } from 'node:path';
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1800 };

export default async function (t) {
  const w = wow64(t);
  const exe = await w.mingw('ftabstale');
  const dll = await w.mingw('ftabdll', { flags: ['-shared', '-nostdlib', '-e', '_DllMain@12'], libs: ['-lkernel32'], out: 'ftabdll.dll' });
  const units = await w.providers(join(w.work, 'units'));
  const modes = [{ GEX_CGEN_PRECOMPILE: '1' }, { GEX_CGEN_PRECOMPILE: '1', GEX_CGEN_GUARD: '1' }];
  await w.unit(exe, units, { modes });
  const page = (name) => w.page(['/t/ftabstale.exe'], { files: { '/t/ftabstale.exe': exe, '/t/ftabdll.dll': dll },
    flags: ['--cgen-prefer'], env: { GEX_CGEN_CACHE: units, GEX_TEST_FTAB_SPARE: '8' }, name });
  const p0 = await page('p0');
  const base = /^ftabdll at (?:0x)?([0-9a-fA-F]+)/m.exec(p0.out)?.[1];
  if (!t.check('page: ftabdll loaded', !!base, w.detail(p0))) return w.clean();
  await w.unit(dll, units, { base: `0x${base}`, modes });
  const p1 = await page('p1');
  for (const i of ['ftabstale.exe', 'ftabdll.dll'])
    t.check(`page: ${i} ran with its unit`, new RegExp(`loaded .*/${i.replace('.', '\\.')} \\(with its unit\\)`).test(p1.out), w.detail(p1, 3));
  t.check('page MP, a thread calls into a unit installed while it ran', /^ftabstale ok /m.test(p1.out), w.detail(p1));
  w.clean();
}
