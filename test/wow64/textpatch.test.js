// A Windows program that patches its own code one write at a time, as an
// old MinGW runtime applies its pseudo-relocations
// (fixtures/wow64/textpatch.c): its .text made writable and executable
// around each write with VirtualProtect, the code doing it in that .text
// and running on after each call. The last patch is seen; and on the page
// the patching code is not translated again for each patch, which said
// "came to translation 64 times" and "LIVELOCK?" and took most of a
// minute a DLL. 64-bit and 32-bit, natively and on the page.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1800 };

const N = 300;
const SUM = `textpatch before 1234567 after ${N - 1}`;
const lines = (out) => out.replace(/\r/g, '').split('\n').filter((l) => l.startsWith('textpatch ')).join('\n');
const again = /came to translation \d+ times|retired \d+ times|LIVELOCK/;

export default async function (t) {
  const w = wow64(t);
  for (const [bits, x64] of [['64', true], ['32', false]]) {
    const exe = await w.mingw('textpatch', { x64, out: `textpatch${bits}.exe` });
    const n = await w.native([`C:\\textpatch${bits}.exe`, String(N)], { files: { [`textpatch${bits}.exe`]: exe }, name: `native-${bits}` });
    if (lines(n.out) === 'textpatch skip') t.skip('the compiler laid getter out otherwise');
    t.eq(`native, ${bits}-bit: the last patch is seen`, lines(n.out), SUM, w.detail(n));
    const p = await w.page([`/t/textpatch${bits}.exe`, String(N)], { files: { [`/t/textpatch${bits}.exe`]: exe }, name: `page-${bits}` });
    t.eq(`page, ${bits}-bit: the last patch is seen`, lines(p.out), SUM, w.detail(p));
    t.check(`page, ${bits}-bit: the patching code is not translated again a patch`, !again.test(p.out), w.detail(p));
  }
  w.clean();
}
