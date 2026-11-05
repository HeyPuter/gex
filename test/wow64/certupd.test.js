// crypt32's context interfaces called through on wasm (certupd.c):
// I_CertUpdateStore frees a context found in both stores through the
// certificate interface's freeContext, whose type must be that of
// CertFreeCertificateContext or the call traps ("indirect call type
// mismatch"), as every WinVerifyTrust of a signed file in a 32-bit process
// did. Natively from an i386 program in a WoW64 process, and on the page.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const w = wow64(t);
  const want = /^certupd ok/m;
  const exe = await w.mingw('certupd', { libs: ['-lcrypt32'] });
  const n = await w.native(['C:\\certupd.exe'], { files: { 'certupd.exe': exe } });
  t.check(`native: ${want.source}`, want.test(n.out), w.detail(n));
  const p = await w.page(['/t/certupd.exe'], { files: { '/t/certupd.exe': exe } });
  t.check(`page: ${want.source}`, want.test(p.out), w.detail(p));
  w.clean();
}
