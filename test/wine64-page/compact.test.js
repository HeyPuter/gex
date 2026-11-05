// 64-bit Wine's programs on the page with the native PE core
// (GEX_TEST_WINE64_NATIVE), with the door: a 64-bit process in a 32-bit
// memory is then in the compact space (KUSER_SHARED_DATA folded under
// 64-bit addressing, the low layout), which the page's split never takes.
// The console set, the first process in its own place, the null pointer's
// locked writes, KUSER_SHARED_DATA by constant and through a computed
// pointer (an access violation there), USER on the null driver, and 24
// DLLs at bases above 4 GiB loaded and unloaded (cases.mjs).
import { CONSOLE, dllcycle, each, kuser, LOWNULL, NULLDRV, starter } from '../wine64/cases.mjs';
import { wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 5400 };

export default async function (t) {
  const w = wine64(t, { door: 1, native: true });
  const o = { page: {}, timeout: 900000 };
  const pre = 'page compact: ';
  await each(w, [...CONSOLE, ...LOWNULL, ...kuser(true), ...NULLDRV], o, pre);
  await starter(t, w, o, pre);
  await dllcycle(t, w, o, pre);
  w.clean();
}
