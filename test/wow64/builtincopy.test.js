// A builtin DLL loaded again from a copy at another path (builtincopy.c,
// ucrtbase: a program that ships its own and loads it by its full path, as
// Zoom's installer does): the loaded DLL comes back with its state, its file
// mode as set and its heap allocating. The page took the live first mapping
// of the DLL for released and started the module's data afresh, so
// ucrtbase's heap handle was NULL and its next calloc hung. An x86_64
// program and an i386 one in a WoW64 process, natively and on the page; with
// GEX_TEST_WOW64_M64_DIST (a WINE64_WOW64=1 WINE64_MEM64=1 install), the
// x86_64 one on the page's memory64 shim too.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const w = wow64(t);
  const want = /^builtincopy ok/m;
  for (const [bits, x64] of [['64', true], ['32', false]]) {
    const exe = await w.mingw('builtincopy', { x64, out: `builtincopy${bits}.exe` });
    const n = await w.native([`C:\\builtincopy${bits}.exe`], { files: { [`builtincopy${bits}.exe`]: exe }, name: `native${bits}` });
    t.check(`native${bits}: ${want.source}`, want.test(n.out), w.detail(n));
    const p = await w.page([`/t/builtincopy${bits}.exe`], { files: { [`/t/builtincopy${bits}.exe`]: exe }, name: `page${bits}`, timeout: 180000 });
    t.check(`page${bits}: ${want.source}`, want.test(p.out), w.detail(p));
    if (x64 && process.env.GEX_TEST_WOW64_M64_DIST) {
      const m = await w.page([`/t/builtincopy${bits}.exe`], { files: { [`/t/builtincopy${bits}.exe`]: exe }, name: `page${bits}-m64`, timeout: 180000, mem64: true, dist: process.env.GEX_TEST_WOW64_M64_DIST });
      t.check(`page${bits} mem64: ${want.source}`, want.test(m.out), w.detail(m));
    }
  }
  w.clean();
}
