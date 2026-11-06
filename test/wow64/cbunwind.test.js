// An exception raised in a window procedure and caught above the
// SendMessage that called it (cbunwind.c), as C++Builder's code does: the
// catch leaves user32's native frames for the callback, and the system
// call's, behind. Many times over, and once past two callbacks. Natively
// from an i386 program in a WoW64 process, and on the page.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const w = wow64(t);
  const want = /^cbunwind ok/m;
  const exe = await w.mingw('cbunwind', { libs: ['-luser32'] });
  const n = await w.native(['C:\\cbunwind.exe'], { files: { 'cbunwind.exe': exe } });
  t.check(`native: ${want.source}`, want.test(n.out), w.detail(n));
  const p = await w.page(['/t/cbunwind.exe'], { files: { '/t/cbunwind.exe': exe } });
  t.check(`page: ${want.source}`, want.test(p.out), w.detail(p));
  w.clean();
}
