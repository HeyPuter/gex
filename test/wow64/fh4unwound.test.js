// A rethrow from a catch block under __CxxFrameHandler4 (fh4unwound.c):
// the frames below the catch were unwound when it was entered, and the
// rethrow's unwind goes on from the frame that caught, so their cleanups
// run once. FH4 keeps no state in a frame: one unwound again runs its
// cleanups again, from another frame's state. x86_64 only (FH4 is x64's),
// natively and on the page.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const w = wow64(t);
  const want = /^fh4 rethrow: caught, cleanups 1/m;
  const exe = await w.mingw('fh4unwound', { out: 'fh4unwound64.exe', x64: true });
  const n = await w.native(['C:\\fh4unwound64.exe'], { files: { 'fh4unwound64.exe': exe }});
  t.check(`native: ${want.source}`, want.test(n.out), w.detail(n));
  const p = await w.page(['/t/fh4unwound64.exe'], { files: { '/t/fh4unwound64.exe': exe } });
  t.check(`page: ${want.source}`, want.test(p.out), w.detail(p));
  w.clean();
}
