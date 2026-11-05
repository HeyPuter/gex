// cases.mjs's deco for an x64 program on 64-bit Wine with the native PE core (GEX_TEST_WINE64_NATIVE), natively under gexc's
// Wayland compositor. The page's half of the path (DOM events to the same
// compositor code) is the browser's.
import { deco } from '../wine64/cases.mjs';
import { wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 1200 };

export default async function (t) {
  const w = wine64(t, { door: null, native: true });
  await deco(t, w, 'x64');
  w.clean();
}
