// A full prefix update under gexc, without the door, on the null driver
// (fixtures/wine/src/prefupdate.c): wineboot -u, setupapi's DefaultInstall
// loading and registering every builtin in one rundll32, registers the DLLs
// it loads last too (windowscodecs, wmadmod, wmvdecod).
import { wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 1800 };

export default async function (t) {
  const w = wine64(t, { door: 0 });
  await w.nt({ name: 'prefupdate', want: /(ok [^\n]*\n)+0 failures/, exe: 'prefupdate.exe', nullDriver: true, timeout: 1500000 });
  w.clean();
}
