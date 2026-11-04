// A pad connected later, while a 32-bit program waits for one
// (test/lib/pads.mjs): winebus started then, once per system, out and back.
import { boot, checkRun, waitRun, xbox } from '../lib/pads.mjs';

export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 1200 };

export default async function (t) {
  const { eq, until } = t;
  const list = [];
  const w = await boot(t, list);
  checkRun(t, 'x86', await waitRun(t, w, list, 'x86', 240, () => {
    eq('not started before the pad', w.starts, []);
    list[0] = xbox(false);
  }));
  eq('started once', w.starts, [0]);
  checkRun(t, 'x64', await waitRun(t, w, list, 'x64', 120));
  // Out and back: still once.
  list.length = 0;
  await until(() => !w.gp.pads.length, 'the pad gone');
  list[0] = xbox(false);
  await until(() => w.gp.pads.length, 'the pad back');
  await new Promise((r) => setTimeout(r, 1000));
  eq('once per system', w.starts, [0]);
  await w.sys.shutdown();
}
