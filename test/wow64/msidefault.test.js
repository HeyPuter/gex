// Enter in an installer's dialog takes the button a ControlCondition makes
// the default (msidefault.c): Wine's msi ran the Dialog table's
// Control_Default whatever the condition named, so an MSI with two variants
// of its Install button (with and without the elevation shield) did nothing
// on Enter. A package of the program's own, one dialog, Enter pressed in it
// once it shows: the conditioned button must answer. Natively and on the
// page, an x86_64 program and an i386 one in a WoW64 process; Wine's null
// display driver (the prefix has none).
import { wow64, graphics } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const w = wow64(t);
  const want = /^msidefault ok/m;
  const userReg = graphics('null');
  for (const [bits, x64] of [['64', true], ['32', false]]) {
    const exe = await w.mingw('msidefault', { x64, out: `msidefault${bits}.exe`, libs: ['-lmsi'] });
    const n = await w.native([`C:\\msidefault${bits}.exe`], { files: { [`msidefault${bits}.exe`]: exe }, userReg, name: `native${bits}` });
    t.check(`native${bits}: ${want.source}`, want.test(n.out), w.detail(n));
    const p = await w.page([`/t/msidefault${bits}.exe`], { files: { [`/t/msidefault${bits}.exe`]: exe }, userReg, name: `page${bits}`, timeout: 240000 });
    t.check(`page${bits}: ${want.source}`, want.test(p.out), w.detail(p));
  }
  w.clean();
}
