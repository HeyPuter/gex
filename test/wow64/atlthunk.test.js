// An ATL window thunk (atlthunk.c): atlthunk.dll writes the guest's own
// thunk code, which replaces the window handle with its argument and jumps
// to the window procedure, called through user32 and directly. Natively
// from an i386 program in a WoW64 process and from the same program built
// for x86_64, and on the page.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const w = wow64(t);
  const want = /^atlthunk ok/m;
  const build = { libs: ['-luser32'] };
  const exe = await w.mingw('atlthunk', build);
  const exe64 = await w.mingw('atlthunk', { ...build, out: 'atlthunk64.exe', x64: true });
  const n = await w.native(['C:\\atlthunk.exe'], { files: { 'atlthunk.exe': exe } });
  t.check(`native: ${want.source}`, want.test(n.out), w.detail(n));
  const n64 = await w.native(['C:\\atlthunk64.exe'], { files: { 'atlthunk64.exe': exe64 }, name: 'native64' });
  t.check(`native64: ${want.source}`, want.test(n64.out), w.detail(n64));
  const p = await w.page(['/t/atlthunk.exe'], { files: { '/t/atlthunk.exe': exe } });
  t.check(`page: ${want.source}`, want.test(p.out), w.detail(p));
  w.clean();
}
