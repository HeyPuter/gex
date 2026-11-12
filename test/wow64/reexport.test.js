// A Wine builtin's export that re-exports a function of a DLL it only
// delay-imports (imagehlp's SymSetOptions, dbghelp's), called through its
// address (fixtures/wow64/reexport.c): it reaches the owner's code. 64-bit
// and 32-bit, natively.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const w = wow64(t);
  for (const [bits, x64] of [['64', true], ['32', false]]) {
    const exe = await w.mingw('reexport', { x64, out: `reexport${bits}.exe` });
    const n = await w.native([`C:\\reexport${bits}.exe`], { files: { [`reexport${bits}.exe`]: exe }, name: `native-${bits}` });
    const got = n.out.replace(/\r/g, '').split('\n').filter((l) => l.startsWith('reexport')).join('\n');
    t.eq(`${bits}-bit: the owner's function runs`, got, 'reexport 10', w.detail(n));
  }
  w.clean();
}
