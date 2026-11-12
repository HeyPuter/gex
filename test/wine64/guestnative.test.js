// Guest x86-64 code calling native code (cases.mjs guestNative says what
// each checks): convmatrix, lateload and cbthrow against a gexcc-win DLL
// (GEX_TEST_GEXCC_WIN), comvt against Wine's own native DLLs; under gexc.
import { guestNative } from './cases.mjs';
import { wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1800 };

export default async function (t) {
  const w = wine64(t, { door: 0 });
  await guestNative(t, w);
  w.clean();
}
