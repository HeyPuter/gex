// msvcp60 loaded (msvcp60load.c): its DllMain's locale calls string
// destructors through a declaration whose type is not theirs, a trap in
// wasm unless the two agree. Natively from an i386 program in a WoW64
// process and from the same program built for x86_64, and on the page.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const w = wow64(t);
  const want = /^msvcp60 ok/m;
  const exe = await w.mingw('msvcp60load');
  const exe64 = await w.mingw('msvcp60load', { out: 'msvcp60load64.exe', x64: true });
  const n = await w.native(['C:\\msvcp60load.exe'], { files: { 'msvcp60load.exe': exe } });
  t.check(`native: ${want.source}`, want.test(n.out), w.detail(n));
  const n64 = await w.native(['C:\\msvcp60load64.exe'], { files: { 'msvcp60load64.exe': exe64 }, name: 'native64' });
  t.check(`native64: ${want.source}`, want.test(n64.out), w.detail(n64));
  const p = await w.page(['/t/msvcp60load.exe'], { files: { '/t/msvcp60load.exe': exe } });
  t.check(`page: ${want.source}`, want.test(p.out), w.detail(p));
  w.clean();
}
