// A pad connected before the boot (test/lib/pads.mjs): winebus started at
// once, the programs see it.
import { boot, checkRun, waitRun, xbox } from '../lib/pads.mjs';

export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 1200 };

export default async function (t) {
  const list = [xbox(false)];
  const w = await boot(t, list);
  await t.until(() => w.starts.length, 'winebus started', 300000);
  t.eq('sc start winebus', w.starts, [0]);
  checkRun(t, 'x64', await waitRun(t, w, list, 'x64', 120));
  checkRun(t, 'x86', await waitRun(t, w, list, 'x86', 120));
  await w.sys.shutdown();
}
