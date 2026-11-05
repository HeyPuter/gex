// An ordinary PE image mapped where a builtin was (overmap.c): the builtin's
// export bindings are taken back when the image is mapped over them, so a
// call there runs the image's own code. Kept, it ran the builtin's native
// export (DXSETUP's dsetup.dll, mapped over the advpack it had freed, ran
// advpack's DelNodeA from its DllMain). Natively from an i386 program in a
// WoW64 process and from the same program built for x86_64, and on the page.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1200 };

export default async function (t) {
  const w = wow64(t);
  const want = /^overmap ok/m;
  const dllFlags = { flags: ['-shared', '-nostdlib', '-e', '_DllMain@12', '-Wl,--disable-dynamicbase,--disable-reloc-section'], libs: ['-lkernel32'], out: 'overdll.dll' };
  const exe = await w.mingw('overmap');
  const dll = await w.mingw('overdll', dllFlags);
  const exe64 = await w.mingw('overmap', { out: 'overmap64.exe', x64: true });
  const dll64 = await w.mingw('overdll', { ...dllFlags, flags: ['-shared', '-nostdlib', '-e', 'DllMain', '-Wl,--disable-dynamicbase,--disable-reloc-section'], out: 'overdll64/overdll.dll', x64: true });
  const n = await w.native(['C:\\overmap.exe'], { files: { 'overmap.exe': exe, 'overdll.dll': dll } });
  t.check(`native: ${want.source}`, want.test(n.out), w.detail(n));
  const n64 = await w.native(['C:\\overmap64.exe'], { files: { 'overmap64.exe': exe64, 'overdll.dll': dll64 }, name: 'native64' });
  t.check(`native64: ${want.source}`, want.test(n64.out), w.detail(n64));
  const p = await w.page(['/t/overmap.exe'], { files: { '/t/overmap.exe': exe, '/t/overdll.dll': dll } });
  t.check(`page: ${want.source}`, want.test(p.out), w.detail(p));
  w.clean();
}
