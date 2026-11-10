// gdi32's D3DKMTSetProcessSchedulingPriorityClass (d3dkmtprio.c): found by
// GetProcAddress and successful for the calling process. A program that
// imports it and finds no export calls Wine's 0xdeadbeef stub and crashes.
// Natively and on the page, x86_64 and i386.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const w = wow64(t);
  const want = /^d3dkmtprio ok/m;
  for (const [bits, x64] of [['64', true], ['32', false]]) {
    const exe = await w.mingw('d3dkmtprio', { x64, out: `d3dkmtprio${bits}.exe` });
    const n = await w.native([`C:\\d3dkmtprio${bits}.exe`], { files: { [`d3dkmtprio${bits}.exe`]: exe }, name: `native${bits}` });
    t.check(`native${bits}: ${want.source}`, want.test(n.out), w.detail(n));
    const p = await w.page([`/t/d3dkmtprio${bits}.exe`], { files: { [`/t/d3dkmtprio${bits}.exe`]: exe }, name: `page${bits}`, timeout: 240000 });
    t.check(`page${bits}: ${want.source}`, want.test(p.out), w.detail(p));
  }
  w.clean();
}
