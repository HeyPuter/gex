// cases.mjs's wheel for a WoW64 process (i686) on the WoW64 install, natively under gexc's
// Wayland compositor. The page's half of the path (DOM events to the same
// compositor code) is the browser's.
import { wheel } from '../wine64/cases.mjs';
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 1200 };

export default async function (t) {
  const w = wow64(t);
  await wheel(t, w, 'x86');
  w.clean();
}
