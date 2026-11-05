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
