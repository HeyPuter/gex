// A table trap no catch-up answers, in a call-out, ends the process as a
// trap, under gexc with the native PE core (GEX_TEST_WINE64_NATIVE):
// table-trap.c calls a page of its own code once, points that page's
// dispatch directory entry past every table's end, and calls it again. Its
// main runs in a call-out, whose table-trap retry once had nothing to
// replay and looped there for good; gexc reports "out of bounds table
// access" and exits 1, well inside 60 s.
import { graphics, wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 600 };

export default async function (t) {
  const w = wine64(t, { door: null, native: true });
  const r = await w.native(['C:\\table-trap.exe'], { files: { 'table-trap.exe': w.prog('table-trap.exe') }, userReg: graphics('null'),
    wineEnv: { USER: 'gex', HOME: '/tmp' }, timeout: 60000, name: 'table-trap' });
  t.check('the first call ran', /^table-trap first 7/m.test(r.out), w.detail(r, 4));
  t.check('the second call ends as a table trap (exit 1), within 60 s', !r.timedOut && r.code === 1 && r.out.includes('out of bounds table access'), w.detail(r, 4));
  t.check('the second call did not return', !/^table-trap second/m.test(r.out));
  w.clean();
}
