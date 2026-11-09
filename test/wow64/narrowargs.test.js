// Narrow integers across the PE boundary (narrowargs.c): a BYTE argument
// with its register's upper bits set reaches Wine as the BYTE, and a char a
// PE callback returns with junk above it in EAX comes back to Wine as the
// char. Wine's modules are wasm, whose C ABI has the caller extend narrow
// values and the callee rely on it; Windows' conventions extend nothing. So
// the ICALL pass (toolchain/sysroot/icall) has every function of Wine's PE
// side extend its own narrow parameters, and every indirect call its narrow
// result. Edge Update's registration took GetSidLengthRequired(2) as 4 GB
// and failed with E_INVALIDARG. Natively and on the page, x86_64 and i386.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const w = wow64(t);
  const want = /^narrowargs ok/m;
  for (const [bits, x64] of [['64', true], ['32', false]]) {
    const exe = await w.mingw('narrowargs', { x64, out: `narrowargs${bits}.exe` });
    const n = await w.native([`C:\\narrowargs${bits}.exe`], { files: { [`narrowargs${bits}.exe`]: exe }, name: `native${bits}` });
    t.check(`native${bits}: ${want.source}`, want.test(n.out), w.detail(n));
    const p = await w.page([`/t/narrowargs${bits}.exe`], { files: { [`/t/narrowargs${bits}.exe`]: exe }, name: `page${bits}`, timeout: 240000 });
    t.check(`page${bits}: ${want.source}`, want.test(p.out), w.detail(p));
  }
  w.clean();
}
