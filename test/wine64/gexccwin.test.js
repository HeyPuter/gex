// gexcc-win programs (native wasm modules beside their .exe) on 64-bit
// Wine under gexc, without the door, with the gexcc-win install
// GEX_TEST_GEXCC_WIN (fixtures/wine64, fixtures/wine/src):
// - cxxcatch: an indirect call inside a catch block, a rethrow, a try in a
//   catch; cxxthr: throws and thread_local data on several threads;
// - dllimps: calls of DLL functions Wine's own native modules do not import;
// - timerproc: timer procedures, the program's own and comctl32's, through
//   DispatchMessage, a message box's loop and the menu loop;
// - lresult: negative and 64-bit LRESULTs through user32's message calls
//   from a program that names no import libraries, as MinGW programs do;
// - cbfault: a wild read in a window procedure under a user callback is an
//   access violation its vectored handler sees, and the process goes on;
// - ptval: user32 calls taking a POINT by value, directly and through
//   GetProcAddress's pointers;
// - selfaddr: GetModuleHandleExW(FROM_ADDRESS) on the program's own data,
//   zeroed data and function is the program's module;
// - bigdata: 24 MiB of zeroed data, more than the page's PE data window
//   for builtins' data exports holds: its ends written and read, and the
//   program's module;
// - missimp: an import no DLL exports, bound to a stub (listed at the
//   link): the program runs, and a call of it ends the process as Wine's
//   stubs do;
// - bigrelocs: more addresses in data than one function of their
//   relocations applies within the engines' function size limit (pelink
//   splits it);
// - hostdatacc: hostdata.c's statics, gex's memory outside every image;
// - wmain0, wmain3: wmain(void) and wmain(argc, argv, envp);
// - xthr: a just-loaded DLL's function called by a thread with no system
//   call since; xhoist: a call loop's hoisted ICALL bound while another
//   thread loads a DLL and points the loop into it.
// The null driver where a window is made.
import { gexccWin } from './cases.mjs';
import { wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 2400 };

export default async function (t) {
  const w = wine64(t, { door: 0 });
  await gexccWin(t, w);
  w.clean();
}
