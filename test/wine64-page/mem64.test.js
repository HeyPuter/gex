// 64-bit Wine's memory64 flavour (wine/dist.sh WINE64_MEM64=1,
// GEX_TEST_WINE64_M64_DIST and _PREFIX) on the page's memory64 shim and
// seed (GEX_MEM64=1), with the door: console programs, and data above
// 4 GiB with code below it (highdata mem64: 5 GiB), and an exe that only
// starts at 0x100000000 (fpcbase: Free Pascal's base, no relocations).
import { allOk, CONSOLE, each } from '../wine64/cases.mjs';
import { wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 5400 };

const pick = (n) => CONSOLE.find((c) => c.name === n);

export default async function (t) {
  const w = wine64(t, { door: 1, mem64: true });
  await each(w, [pick('hello_args'), pick('crtmem'), pick('tls'), pick('seh'), pick('seh-localunwind'), { name: 'highdata', want: allOk, exe: 'highdata.exe', args: ['mem64'] },
    { name: 'fpcbase', want: allOk, exe: 'fpcbase.exe' }],
    { page: { mem64: true }, timeout: 1800000 }, 'page mem64: ');
  w.clean();
}
