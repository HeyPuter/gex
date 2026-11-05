// Region entries of a program's unit (regions.c): the exe's unit is built with
// a region entry at reg_mid, an address inside a function its own code calls
// through a computed pointer. The entry must be installed, natively (where a
// program's own images take their units with GEX_WINE32_UNITS=all) and on the
// page's shim, and on the page the calls must be served there: none leaves
// for a call-out. Under WoW64 the exe is the process's other mode, so its
// region entries are that mode's, bridged in its directory as its unit's
// entries are.
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { run, wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1800 };

export default async function (t) {
  const w = wow64(t);
  const exe = await w.mingw('regions', { flags: ['-Wall'] });
  const nm = await run([w.tool('i686-w64-mingw32-nm'), exe], { file: join(w.work, 'nm.log') });
  const mid = /^([0-9a-f]+) T _reg_mid$/m.exec(nm.out)?.[1];
  if (!t.check('regions.exe has _reg_mid', !!mid, nm.out.slice(-200))) return w.clean();
  const entries = join(w.work, 'entries.txt');
  writeFileSync(entries, `${mid}\n`);
  const units = join(w.work, 'units');
  await w.providers(units);
  await w.unit(exe, units, { more: ['--entries', entries] });
  const verdict = (leg, r, page) => {
    const why = [];
    const got = r.out.split('\n').map((l) => l.replace(/\r$/, '')).find((l) => /^regions \d+ calls/.test(l));
    if (got !== 'regions 5000 calls, 0 wrong' || !/^regions done/m.test(r.out)) why.push(`output: ${got ?? 'none'}`);
    if (!/function tier for .*regions\.exe/.test(r.out)) why.push('ran without its unit');
    if (!/gex-pe: region entries for .*regions\.exe: [1-9]/.test(r.out)) why.push('its region entries were not installed');
    if (page && new RegExp(`\\[callout\\] #\\d+ depth \\d+: ${parseInt(mid, 16).toString(16)} returning`).test(r.out)) why.push('calls to reg_mid left the unit');
    t.check(`${leg}: region entries installed and served`, why.length === 0, `${why.join('; ')}; ${w.detail(r, 3)}`);
  };
  verdict('native', await w.native(['C:\\regions.exe'], { files: { 'regions.exe': exe },
    env: { GEX_CGEN_CACHE: units, GEX_WINE32_UNITS: 'all', GEX_LOG: 'unit' }, timeout: 900000 }), false);
  verdict('page', await w.page(['/t/regions.exe'], { files: { '/t/regions.exe': exe }, flags: ['--cgen-prefer'],
    env: { GEX_LOG: 'unit', GEX_DENSE_DEBUG: '1', GEX_CGEN_CACHE: units }, timeout: 900000 }), true);
  w.clean();
}
