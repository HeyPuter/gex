// __CxxFrameHandler4 (fh4catch.c): a catch(...) whose try block starts at
// state 1 is found from the frame's ip map. vcruntime140_1's handler keeps
// the search state in an FLS slot its DllMain allocates; without it every
// frame read state 0 and the catch never matched. x86_64 only (FH4 is
// x64's), natively and on the page.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const w = wow64(t);
  const want = /^fh4 caught/m;
  const exe = await w.mingw('fh4catch', { out: 'fh4catch64.exe', x64: true });
  const n = await w.native(['C:\\fh4catch64.exe'], { files: { 'fh4catch64.exe': exe } });
  t.check(`native: ${want.source}`, want.test(n.out), w.detail(n));
  const p = await w.page(['/t/fh4catch64.exe'], { files: { '/t/fh4catch64.exe': exe } });
  t.check(`page: ${want.source}`, want.test(p.out), w.detail(p));
  w.clean();
}
