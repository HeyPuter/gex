// A loopback TCP pair as libcurl's resolver makes one (loopsock.c): the
// accepted end's peer must be the connecting end. On the page the accepted
// end said port 0 (the connecting process kept its port to itself), so
// libcurl threw the pair away and every lookup failed with "getaddrinfo()
// thread failed to start". Natively and on the page, x86_64 and i386.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const w = wow64(t);
  const want = /^loopsock ok/m;
  for (const [bits, x64] of [['64', true], ['32', false]]) {
    const exe = await w.mingw('loopsock', { x64, out: `loopsock${bits}.exe`, libs: ['-lws2_32'] });
    const n = await w.native([`C:\\loopsock${bits}.exe`], { files: { [`loopsock${bits}.exe`]: exe }, name: `native${bits}` });
    t.check(`native${bits}: ${want.source}`, want.test(n.out), w.detail(n));
    const p = await w.page([`/t/loopsock${bits}.exe`], { files: { [`/t/loopsock${bits}.exe`]: exe }, name: `page${bits}`, timeout: 240000 });
    t.check(`page${bits}: ${want.source}`, want.test(p.out), w.detail(p));
  }
  w.clean();
}
