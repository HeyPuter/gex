// A DLL's exports that a static library it imports defines rather than its
// own code (a .spec's `extern` of libuuid's data: msftedit's
// IID_IRichEditOle and IID_IRichEditOleCallback) are in the native wasm32
// module, for i386 and x86-64 programs, natively and on the page
// (staticexport.c), and say no "data export ... is not in Wine's PE
// module".
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const w = wow64(t);
  for (const x64 of [false, true]) {
    const arch = x64 ? 'x86-64' : 'i386';
    const exe = await w.mingw('staticexport', { x64, out: `staticexport-${x64 ? 64 : 32}.exe` });
    const name = `staticexport-${x64 ? 64 : 32}.exe`;
    const n = await w.native([`C:\\${name}`], { files: { [name]: exe }, name: `n-${arch}` });
    t.check(`native ${arch}: staticexport ok`, /^staticexport ok/m.test(n.out), w.detail(n));
    t.check(`native ${arch}: no missing data export`, !/data export \S+ is not in Wine's PE module/.test(n.out), w.detail(n));
    const p = await w.page([`/t/${name}`], { files: { [`/t/${name}`]: exe }, name: `p-${arch}` });
    t.check(`page ${arch}: staticexport ok`, /^staticexport ok/m.test(p.out), w.detail(p));
    t.check(`page ${arch}: no missing data export`, !/data export \S+ is not in Wine's PE module/.test(p.out), w.detail(p));
  }
  w.clean();
}
