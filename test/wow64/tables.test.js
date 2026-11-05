// Dispatch pages numbered past the first dispatch table, natively
// (tables.c): a large unit fills the first table and the pages numbered
// after it land in later ones, where every bridge, call-out watch and
// tier-1 page must reach as well (a zero directory entry, a retranslation
// livelock; a return to the pad's int3 filler at 0x3ffff1). Each table here
// holds GEX_DISPATCH_TABLE_SLOTS slots, so the pages numbered at run time
// start in the second; a run with the full tables is the control. Prints
// "tables ok N" per run.
import { join } from 'node:path';
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1800 };

const SLOTS = 262144;

export default async function (t) {
  const w = wow64(t);
  const exe = await w.mingw('tables');
  const dll = await w.mingw('tablesdll', { flags: ['-shared', '-nostdlib', '-e', '_DllMain@12'], libs: ['-lkernel32'], out: 'tablesdll.dll' });
  const units = await w.providers(join(w.work, 'units'));
  await w.unit(exe, units, { modes: [{ GEX_CGEN_PRECOMPILE: '1' }] });
  const run = async (leg, more, name) => {
    const r = await w.native(['C:\\tables.exe'], { files: { 'tables.exe': exe, 'tablesdll.dll': dll },
      env: { GEX_CGEN_CACHE: units, GEX_WINE32_UNITS: 'all', GEX_WINE32_TRACE: '1', ...more }, name });
    t.check(`${leg}: ran with its unit`, !/tables\.exe at 0x[0-9a-f]*: no function-tier unit/.test(r.out), w.detail(r, 3));
    t.check(`${leg}: tables ok`, /^tables ok [1-9]/m.test(r.out), w.detail(r));
    return r;
  };
  await run('full tables', {}, 'full');
  const s = await run(`tables of ${SLOTS} slots`, { GEX_DISPATCH_TABLE_SLOTS: String(SLOTS) }, 'small');
  t.check('the cap was taken', s.out.includes(`dispatch tables of ${SLOTS} slots`), w.detail(s, 3));
  w.clean();
}
