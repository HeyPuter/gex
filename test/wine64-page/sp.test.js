// 64-bit Wine's programs on the page's single-thread build, as a page
// without SharedArrayBuffer runs it (GEX_SINGLE=1, JSPI; no door): the
// console set, the null pointer's locked writes, win32u's session reads
// against another thread's writes (2000 writes a case, every read a server
// request), and with the native PE core GEX_TEST_WINE64_NATIVE native DLLs
// freed and loaded again at other bases (dllreload).
import { allOk, CONSOLE, each, LOWNULL } from '../wine64/cases.mjs';
import { wine64 } from '../lib/winerun.mjs';
import { Skip } from '../lib/t.js';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 7200 };

export default async function (t) {
  const o = { page: { single: true }, timeout: 900000 };
  const pre = 'page sp: ';
  const w = wine64(t, { door: 0 });
  await each(w, [...CONSOLE, ...LOWNULL, { name: 'seqrace', want: allOk, exe: 'seqrace.exe', args: ['2000', '7'] }], o, pre);
  w.clean();
  let n;
  try { n = wine64(t, { door: 0, native: true }); } catch (e) {
    if (!(e instanceof Skip)) throw e;
    return t.say(`${pre}dllreload: ${e.message}`);
  }
  await n.nt({ ...o, name: `${pre}dllreload (native PE core)`, want: allOk, exe: 'dllreload.exe' });
  n.clean();
}
