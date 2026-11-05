// Guest callbacks from native code nested deep (cbdepth.c: qsort whose
// comparator sorts again), on tier-1 (the provider modules only), natively
// (gexc run) and on the page's shim: 100 and 450 levels must all return
// ("cbdepth ok N", exit 0). With a native i386 side each level is a
// call-out, so 8000, past what the thread's stack or the host's has room
// for, and 4000 on a thread whose stack is 1 MiB, must end the process with
// the "nest deeper" line and exit 253 (STATUS_STACK_OVERFLOW's low byte),
// not hang nor run native frames on past the stack's end; with a translated
// one qsort is i386 msvcrt's own guest code, and there is no such leg. Then
// on the page with the exe's unit, where the levels are direct calls: 1000
// levels, past the units' depth guard, must all return.
import { cpSync } from 'node:fs';
import { join } from 'node:path';
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 2400 };

export default async function (t) {
  const w = wow64(t);
  const exe = await w.mingw('cbdepth');
  const units = await w.providers(join(w.work, 'units'));
  const withUnit = join(w.work, 'units-u');
  cpSync(units, withUnit, { recursive: true });
  await w.unit(exe, withUnit, { modes: [{ GEX_CGEN_GUARD: '1' }] });
  const check = (leg, r, line, code) =>
    t.check(`${leg}: ${line.source}, exit ${code}`, line.test(r.out) && r.code === code, w.detail(r));
  const page = (args, cache, name) => w.page(['/t/cbdepth.exe', ...args], { files: { '/t/cbdepth.exe': exe },
    flags: ['--cgen-prefer'], env: { GEX_CGEN_CACHE: cache }, name });
  const leg = async (depth, line, code, thread) => {
    const args = [depth, ...(thread ? ['thread'] : [])];
    const on = thread ? ' on a thread' : '';
    check(`native, ${depth} levels${on}`, await w.native(['C:\\cbdepth.exe', ...args], { files: { 'cbdepth.exe': exe },
      env: { GEX_CGEN_CACHE: units }, name: `native-${depth}` }), line, code);
    check(`page, ${depth} levels${on}`, await page(args, units, `page-${depth}`), line, code);
  };
  await leg(100, /^cbdepth ok 100/m, 0);
  await leg(450, /^cbdepth ok 450/m, 0);
  if (w.i386Native) {
    await leg(8000, /guest calls nest deeper than/, 253);
    await leg(4000, /native Wine's guest calls nest deeper than the thread's stack/, 253, true);
  }
  const u = await page([1000], withUnit, 'page-units');
  t.check('page, units: ran with its unit', /cbdepth\.exe \(with its unit\)/.test(u.out), w.detail(u));
  check('page, units, 1000 levels', u, /^cbdepth ok 1000/m, 0);
  w.clean();
}
