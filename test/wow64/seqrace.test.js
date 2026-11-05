// win32u's session reads against another thread's writes: the 64-bit Wine
// test's program (fixtures/wine/src/seqrace.c) built for i686, as a WoW64
// process. Natively with the door (20000 writes a case) and without it
// (WINEGEX_DOOR=0), and on the page's shim, where every read is a server
// request (2000 a case). Each prints "0 failures".
import { join } from 'node:path';
import { ROOT, wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 3000 };

export default async function (t) {
  const w = wow64(t);
  const exe = await w.mingw('seqrace', { src: join(ROOT, 'test/fixtures/wine/src/seqrace.c') });
  const want = /^0 failures/m;
  const fails = (r) => `${r.out.split('\n').filter((l) => /^FAIL|failures/.test(l)).slice(-8).join(' / ')}; ${w.detail(r, 3)}`;
  for (const door of [1, 0]) {
    const r = await w.native(['C:\\seqrace.exe', door ? 20000 : 2000], { files: { 'seqrace.exe': exe },
      wineEnv: { WINEGEX_DOOR: String(door) }, timeout: 900000, name: `native-${door}` });
    t.check(`native door=${door}: 0 failures`, want.test(r.out), fails(r));
  }
  const p = await w.page(['/t/seqrace.exe', 2000], { files: { '/t/seqrace.exe': exe }, timeout: 900000 });
  t.check('page: 0 failures', want.test(p.out), fails(p));
  w.clean();
}
