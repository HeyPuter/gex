// user32's wsprintfA/W (wsprintf.c): varargs exports whose .spec calls the
// buffer a string too; the x86-64 adapter must take the format, the last
// string, to lay the arguments out (taking the buffer, every argument read
// 0, and NSIS's IntOp, which formats with wsprintfW, never counted).
// Natively from an i386 program in a WoW64 process and from the same
// program built for x86_64, and on the page.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const w = wow64(t);
  const want = /^wsprintf ok/m;
  const exe = await w.mingw('wsprintf', { libs: ['-luser32'] });
  const exe64 = await w.mingw('wsprintf', { out: 'wsprintf64.exe', x64: true, libs: ['-luser32'] });
  const n = await w.native(['C:\\wsprintf.exe'], { files: { 'wsprintf.exe': exe } });
  t.check(`native: ${want.source}`, want.test(n.out), w.detail(n));
  const n64 = await w.native(['C:\\wsprintf64.exe'], { files: { 'wsprintf64.exe': exe64 }, name: 'native64' });
  t.check(`native64: ${want.source}`, want.test(n64.out), w.detail(n64));
  const p = await w.page(['/t/wsprintf64.exe'], { files: { '/t/wsprintf64.exe': exe64 } });
  t.check(`page: ${want.source}`, want.test(p.out), w.detail(p));
  w.clean();
}
