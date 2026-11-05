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
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { Skip } from '../lib/t.js';
import { ROOT, wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 2400 };

const allOk = /(ok [^\n]*\n)+0 failures/;
const F = join(ROOT, 'test/fixtures/wine64');
const W = join(ROOT, 'test/fixtures/wine/src');

export default async function (t) {
  const w = wine64(t, { door: 0 });
  const cc = (src, out, args) => w.gexccWin(src, out, args);
  const one = async (name, src, args, c = {}) => {
    let exe;
    try { exe = await cc(src, `${name}.exe`, args); } catch (e) {
      if (e instanceof Skip) throw e;
      return t.check(`${name}: build`, false, String(e.message));
    }
    const { extra = [], ...more } = c;
    await w.nt({ name, want: allOk, exe, extra: [`${exe}.wasm`, ...extra], ...more });
  };
  await one('cxxcatch', join(F, 'cxxcatch.cpp'), ['-O2']);
  await one('dllimps', join(F, 'dllimps.c'), ['-O2']);
  await one('cxxthr', join(F, 'cxxthr.cpp'), ['-O2']);
  await one('timerproc', join(W, 'timerproc.c'), ['-O1', '-municode', '-lcomctl32'], { nullDriver: true });
  await one('lresult', join(F, 'lresult.c'), ['-O1', '-mwindows'], { nullDriver: true });
  await one('cbfault', join(F, 'cbfault.c'), ['-O1'], { nullDriver: true,
    want: /cbfault: sending\ncbfault: veh c0000005\n([^\n]*\n)*cbfault: returned 0, veh [0-9]+/ });
  await one('ptval', join(F, 'ptval.c'), ['-O1', '-mwindows'], { nullDriver: true });
  await one('hostdatacc', join(W, 'hostdata.c'), ['-O2']);
  // wmain(void) compiled and linked in one step; wmain(argc, argv, envp)
  // compiled first (-c), then linked
  const w0 = join(w.work, 'wmain0.c');
  writeFileSync(w0, '#include <stdio.h>\nint wmain(void) { printf("wmain0 ok\\n"); return 40; }\n');
  await one('wmain0', w0, ['-O1', '-municode'], { code: 40, want: /wmain0 ok/ });
  const w3 = join(w.work, 'wmain3.c');
  writeFileSync(w3, '#include <stdio.h>\nint wmain(int c, unsigned short **v, unsigned short **e) { printf("wmain3 ok %d %s\\n", c, e && e[0] ? "env" : "noenv"); return 43; }\n');
  const o3 = await cc(w3, 'wmain3.o', ['-O1', '-c']);
  await one('wmain3', o3, ['-municode'], { code: 43, want: /wmain3 ok 1 env/ });
  // xthr_dll.dll beside both
  let dll;
  try { dll = await cc(join(F, 'xthr_dll.c'), 'xthr_dll.dll', ['-O2', '-shared', join(F, 'xthr_dll.def')]); } catch (e) {
    if (e instanceof Skip) throw e;
    return t.check('xthr_dll: build', false, String(e.message));
  }
  for (const n of ['xthr', 'xhoist']) await one(n, join(F, `${n}.c`), ['-O2'], { extra: [dll, `${dll}.wasm`] });
  w.clean();
}
