// `throw;` in a catch(...) as MSVC's statically linked C++ runtime runs it
// (fh4rethrow.c): the catch block runs from RtlUnwindEx's consolidate
// callback, and its rethrow is dispatched from there to an outer catch.
// On wasm64 the callback was called straight from RtlRestoreContext, so the
// dispatch went back through the frames the catch had unwound. x86_64
// only, natively and on the page.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const w = wow64(t);
  const want = /^fh4 rethrow caught/m;
  const exe = await w.mingw('fh4rethrow', { out: 'fh4rethrow64.exe', x64: true });
  const n = await w.native(['C:\\fh4rethrow64.exe'], { files: { 'fh4rethrow64.exe': exe } });
  t.check(`native: ${want.source}`, want.test(n.out), w.detail(n));
  const p = await w.page(['/t/fh4rethrow64.exe'], { files: { '/t/fh4rethrow64.exe': exe } });
  t.check(`page: ${want.source}`, want.test(p.out), w.detail(p));
  w.clean();
}
