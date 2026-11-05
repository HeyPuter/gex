// 64-bit Wine's native PE core under gexc, without the door
// (fixtures/wine/src): GetModuleHandleExW(FROM_ADDRESS) on a native
// builtin's static data and window procedures (fromaddr); a builtin's and
// the program's statics as committed memory a server call's reply lands in
// (hostdata); another thread's context, with its SSE state, inside a Unix
// call and in guest code (unixctx); a debugger's break-ins into a running
// child (breakin). With the native core (its DLLs skeletons) also a
// skeleton's view unmapped and its addresses reused for the program's code
// (stalebind), and native DLLs freed and loaded again at other bases, as
// the Start menu's Control Panel applets are (dllreload).
import { wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 2400 };

const allOk = /(ok [^\n]*\n)+0 failures/;

export default async function (t) {
  const w = wine64(t, { door: 0 });
  for (const n of ['fromaddr', 'hostdata', 'unixctx', 'breakin']) await w.nt({ name: n, want: allOk, exe: `${n}.exe` });
  if (!w.nativeCore) t.say('stalebind, dllreload: the install\'s x86_64 side is translated, not the native PE core');
  else for (const n of ['stalebind', 'dllreload']) await w.nt({ name: n, want: allOk, exe: `${n}.exe` });
  w.clean();
}
