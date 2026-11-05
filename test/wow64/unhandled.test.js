// Exceptions nothing handles (unhandled.c): each case's process ends with the
// exception's code, after the program's top-level filter, whichever thread
// raised it and whatever native frames lie between, as on Windows. Natively
// (gexc run) and on the page's shim with the exe's unit (its faults
// delivered in place, under the native frames), without it (tier-1) and
// with the function JIT, as a WoW64 process of 64-bit Wine. Natively and on
// tier-1 only cases 1-12: their guest calls from native code nest one
// call-out each, and past the call-out depth cap such a nest does not come
// back. No exception may reach the run loop with its handler's frame gone.
import { join } from 'node:path';
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1800 };

// Case 13 (an SEH handler that always faults) ends with the fault's own code
// under WoW64; 32-bit Wine ends the thread on the stack's last page (1).
const WANT = ['c000001d', 'c000001d', '00000055', 'c000001d', 'e0001234', 'c000001d', 'c000001d',
  '80000003', 'c0000094', 'c0000005', 'c000001d', 'c000001d', 'c000001d', 'c000001d']
  .map((code, i) => `case ${i + 1} exit ${code}`);

export default async function (t) {
  const w = wow64(t);
  const exe = await w.mingw('unhandled');
  const units = join(w.work, 'units');
  const tier1 = join(w.work, 'units-t1');
  await w.providers(units);
  await w.unit(exe, units);
  await w.providers(tier1);
  const verdict = (leg, r, cases) => {
    const got = r.out.split('\n').map((l) => l.replace(/\r$/, '')).filter((l) => /^case \d+ exit/.test(l)).slice(0, cases);
    const lost = r.out.split('\n').filter((l) => l.includes('handler frame that caught it is gone')).length;
    t.eq(`${leg}: cases 1-${cases}`, got.join('; '), WANT.slice(0, cases).join('; '));
    t.check(`${leg}: no lost handler frames, unhandled done`, lost === 0 && /^unhandled done/m.test(r.out), `${lost} lost; ${w.detail(r)}`);
  };
  const files = { 'unhandled.exe': exe };
  verdict('native', await w.native(['C:\\unhandled.exe', '-12'], { files, env: { GEX_CGEN_CACHE: units }, timeout: 900000 }), 12);
  const page = (cache, last, more = {}, name = 'page') => w.page(['/t/unhandled.exe', `-${last}`], {
    files: { '/t/unhandled.exe': exe }, flags: ['--cgen-prefer'], env: { GEX_LOG: 'unit', GEX_CGEN_CACHE: cache, ...more }, timeout: 900000, name });
  const p = await page(units, 14);
  t.check('page: ran with its unit', /function tier for .*\/unhandled\.exe/.test(p.out), w.detail(p));
  verdict('page', p, 14);
  verdict('page, tier-1', await page(tier1, 12, {}, 'page-t1'), 12);
  verdict('page, function JIT', await page(tier1, 14, { GEX_FJIT: '1' }, 'page-fjit'), 14);
  w.clean();
}
