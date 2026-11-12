// cases.mjs guestNative on the page: guest x86-64 code calling the native
// PE core's modules and a gexcc-win DLL's (GEX_TEST_WINE64_NATIVE,
// GEX_TEST_GEXCC_WIN), each thread a Worker with its own table.
import { guestNative } from '../wine64/cases.mjs';
import { wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 2400 };

export default async function (t) {
  const w = wine64(t, { door: 1, native: true });
  await guestNative(t, w, { page: {}, timeout: 900000 }, 'page: ');
  w.clean();
}
