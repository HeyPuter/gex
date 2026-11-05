// .NET in a WoW64 process (dotnet.cs): the install carries Wine Mono, both
// halves (GEX_TEST_WOW64_MONO), which mscoree finds through WINEDATADIR. The
// program is compiled by Wine Mono's own C# compiler (mcs.exe, itself .NET:
// AnyCPU, so a 64-bit process of translated x86-64 Mono, about 1100 s
// natively) for x86, then run as the WoW64 process natively (gexc run) and
// on the page's shim. Prints "dotnet ok".
import { copyFileSync, existsSync, openSync, readSync, closeSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { FIXTURES, wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 4800 };

export default async function (t) {
  const w = wow64(t, { mono: true });
  const monoTop = join(w.dist, 'share/wine/mono');
  const mono = existsSync(monoTop) && readdirSync(monoTop).filter((d) => d.startsWith('wine-mono-')).sort()[0];
  if (!t.check('Wine Mono in the install', !!mono, monoTop)) return w.clean();
  const m = join(monoTop, mono);
  if (!t.check('Wine Mono\'s x86-64 half (mcs runs as a 64-bit process)', existsSync(join(m, 'bin/libmono-2.0-x86_64.dll')), m)) return w.clean();
  const winpath = (p) => `Z:${p.replaceAll('/', '\\')}`;
  const c = await w.native([winpath(join(m, 'lib/mono/4.5/mcs.exe')), '-platform:x86', '-out:C:\\dotnet.exe', 'C:\\dotnet.cs'],
    { files: { 'dotnet.cs': join(FIXTURES, 'dotnet.cs') }, timeout: 2400000, name: 'mcs' });
  // a whole image, not the empty file a killed mcs leaves behind
  const exe = join(w.work, 'dotnet.exe');
  const built = join(c.pfx, 'drive_c/dotnet.exe');
  const mz = (() => { try { const fd = openSync(built, 'r'); const b = Buffer.alloc(2); readSync(fd, b, 0, 2, 0); closeSync(fd); return b.toString('latin1') === 'MZ'; } catch { return false; } })();
  if (!t.check('mcs compiles dotnet.cs', mz, w.detail(c))) return w.clean();
  copyFileSync(built, exe);
  const want = /^dotnet ok/m;
  const n = await w.native(['C:\\dotnet.exe'], { files: { 'dotnet.exe': exe }, timeout: 900000 });
  t.check(`native: ${want.source}`, want.test(n.out), w.detail(n));
  const p = await w.page(['/t/dotnet.exe'], { files: { '/t/dotnet.exe': exe }, timeout: 900000 });
  t.check(`page: ${want.source}`, want.test(p.out), w.detail(p));
  w.clean();
}
