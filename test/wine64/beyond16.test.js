// A program that reserves memory at a fixed address past 16 GiB, more than
// any gex memory reaches (reserve32g.c: 8 GiB at 32 GiB, as MSYS2's and
// Cygwin's runtime does for its heap), under gexc with the native PE core
// (GEX_TEST_WINE64_NATIVE): the reservation fails, and Wine says once that
// the program needs more than 16 GiB of address space, naming it and the
// reservation, instead of the program failing later with nothing said.
import { graphics, wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 600 };

export default async function (t) {
  const w = wine64(t, { door: null, native: true });
  const r = await w.native(['C:\\reserve32g.exe'], { files: { 'reserve32g.exe': w.prog('reserve32g.exe') }, userReg: graphics('null'),
    wineEnv: { USER: 'gex', HOME: '/tmp' }, timeout: 120000, name: 'reserve32g' });
  t.check('the reservation fails', /^reserve32g refused, error \d+/m.test(r.out), w.detail(r, 4));
  const said = r.out.split('\n').filter((l) => /needs more than 16 GiB of address space/.test(l));
  t.check('Wine says why, once', said.length === 1 && /reserve32g\.exe/.test(said[0]) && /a reservation at 0x800000000 of 8192 MiB is beyond it/.test(said[0]),
    said.join(' / ') || w.detail(r, 4));
  w.clean();
}
