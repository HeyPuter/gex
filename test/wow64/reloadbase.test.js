// A wasm DLL loaded again after its first copy was freed and its old base
// taken by other memory (reloadbase.c, crypt32: Telegram's case): the
// skeleton at the new base is a first mapping, so the module's image base
// is the new one and its DllMain's unix call setup succeeds. From an x86_64
// program in a WoW64 process (x86_64 DLLs are the wasm ones), natively and
// on the page.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const w = wow64(t);
  const want = /^reloadbase ok/m;
  const exe = await w.mingw('reloadbase', { x64: true });
  const n = await w.native(['C:\\reloadbase.exe'], { files: { 'reloadbase.exe': exe }, name: 'native64' });
  t.check(`native64: ${want.source}`, want.test(n.out), w.detail(n));
  const p = await w.page(['/t/reloadbase.exe'], { files: { '/t/reloadbase.exe': exe }, name: 'page64' });
  t.check(`page64: ${want.source}`, want.test(p.out), w.detail(p));
  w.clean();
}
