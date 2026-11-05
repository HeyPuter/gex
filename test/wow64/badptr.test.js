// A system call's native code faulting on a bad pointer its caller passed
// (badptr.c): the call fails with STATUS_ACCESS_VIOLATION and the caller
// goes on. With bad-pointer guards on (GEX_NATIVE_TRY=1): the i386 program
// natively and on the page, and the same program built for x86_64 natively.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const w = wow64(t);
  const env = { GEX_NATIVE_TRY: '1' };
  const want = /^badptr ok/m;
  const exe = await w.mingw('badptr', { flags: ['-fno-omit-frame-pointer'], libs: ['-lntdll'] });
  const exe64 = await w.mingw('badptr', { libs: ['-lntdll'], out: 'badptr64.exe', x64: true });
  const n = await w.native(['C:\\badptr.exe'], { files: { 'badptr.exe': exe }, env });
  t.check(`native: ${want.source}`, want.test(n.out), w.detail(n));
  const n64 = await w.native(['C:\\badptr64.exe'], { files: { 'badptr64.exe': exe64 }, env, name: 'native64' });
  t.check(`native64: ${want.source}`, want.test(n64.out), w.detail(n64));
  const p = await w.page(['/t/badptr.exe'], { files: { '/t/badptr.exe': exe }, env });
  t.check(`page: ${want.source}`, want.test(p.out), w.detail(p));
  w.clean();
}
