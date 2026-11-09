// 64-bit Wine's console programs (fixtures/wine/src, the win-progs image)
// under gexc: each must exit with its code and print exactly what it
// should, most a line per check and "0 failures". Each with the door
// (WINEGEX_DOOR=1, the LP64 door call, as the page runs Wine) and without
// it. highdata: data above 2 GiB, code below it, and Wine's log line for
// the protection it refused.
import { allOk, CONSOLE } from './cases.mjs';
import { wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 3600 };

const CASES = [...CONSOLE, { name: 'highdata', want: allOk, exe: 'highdata.exe' }];

export default async function (t) {
  for (const door of [1, 0]) {
    const w = wine64(t, { door });
    for (const c of CASES) {
      const r = await w.nt({ ...c, name: `${c.name} door=${door}` });
      if (c.name === 'highdata' && r)
        t.check(`highdata door=${door}: Wine says why it refused the protection`, /memory made executable \S+ outside gex's code space/.test(r.err), w.detail(r, 3));
    }
    w.clean();
  }
}
