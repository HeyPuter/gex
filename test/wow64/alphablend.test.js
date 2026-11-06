// AlphaBlend through msimg32 (alphablend.c): msimg32's AlphaBlend is a
// forward to gdi32's GdiAlphaBlend, whose BLENDFUNCTION, a 4-byte struct
// passed by value, wasm takes by reference: each, directly and through
// GetProcAddress, must blend as Windows does (constant alpha 255 and 128,
// and the source's own). Natively from an i386 program in a WoW64 process
// and from the same program built for x86_64, and on the page.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const w = wow64(t);
  const want = /^alphablend ok/m;
  const exe = await w.mingw('alphablend', { libs: ['-lmsimg32', '-lgdi32'] });
  const exe64 = await w.mingw('alphablend', { out: 'alphablend64.exe', x64: true, libs: ['-lmsimg32', '-lgdi32'] });
  const n = await w.native(['C:\\alphablend.exe'], { files: { 'alphablend.exe': exe } });
  t.check(`native: ${want.source}`, want.test(n.out), w.detail(n));
  const n64 = await w.native(['C:\\alphablend64.exe'], { files: { 'alphablend64.exe': exe64 }, name: 'native64' });
  t.check(`native64: ${want.source}`, want.test(n64.out), w.detail(n64));
  const p = await w.page(['/t/alphablend.exe'], { files: { '/t/alphablend.exe': exe } });
  t.check(`page: ${want.source}`, want.test(p.out), w.detail(p));
  w.clean();
}
