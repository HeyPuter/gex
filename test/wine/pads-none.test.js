// No pad (test/lib/pads.mjs): winebus never starts, no driver process for
// it, the programs see no controller.
import { boot, checkRun, procs } from '../lib/pads.mjs';

export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 1200 };

export default async function (t) {
  const w = await boot(t, []);
  checkRun(t, 'x64', await w.run('padtest.exe', ['none']));
  checkRun(t, 'x86', await w.run('padtest.exe', ['none'], { arch: 'x86' }));
  t.eq('winebus not started', [w.wc.state, w.starts], ['idle', []]);
  const ps = await procs(w.fs);
  t.check('no sc, no winebus', !ps.some((p) => /\bsc(\.exe)?\b|winebus/i.test(p)), JSON.stringify(ps));
  await w.sys.shutdown();
}
