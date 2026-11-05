// 64-bit Wine's programs on the page (run-node.mjs, the x86 shared shim
// and seed, wine/page-prefix.sh's prefix), with the door and run-node's
// audio player and its loopback: the console set, the null pointer's
// locked writes, USER on the null driver, win32u's session reads against
// another thread's writes (seqrace, 2000 writes a case), the first process
// that runs its program in its own place (starter), 24 DLLs loaded and
// unloaded with the peak RSS and dispatch slots they keep (dllcycle), the
// ASIO host with its round trip, the translation cache across loads
// (xcache), a program to its exit with nothing translated at run time
// (aot: the page's aot=1, units built from a first run) and a full prefix
// update (cases.mjs says what each checks).
import { aot, asio, CONSOLE, dllcycle, each, LOWNULL, NULLDRV, starter, xcache, allOk } from '../wine64/cases.mjs';
import { wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 5400 };

export default async function (t) {
  const w = wine64(t, { door: 1 });
  const o = { page: {}, env: { GEX_AUDIO_PLAYER: '1', GEX_AUDIO_LOOP: '1' }, timeout: 900000 };
  const pre = 'page mp: ';
  await each(w, [...CONSOLE, ...LOWNULL, ...NULLDRV, { name: 'seqrace', want: allOk, exe: 'seqrace.exe', args: ['2000', '7'] }], o, pre);
  await starter(t, w, o, pre);
  await dllcycle(t, w, o, pre, { rss: true });
  await asio(t, w, o, pre);
  await xcache(t, w, o, pre);
  await aot(t, w, o, pre);
  await w.nt({ ...o, name: `${pre}prefupdate`, want: allOk, exe: 'prefupdate.exe', nullDriver: true, timeout: 1500000 });
  w.clean();
}
