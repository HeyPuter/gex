// A program linked with the linker's strip options, as GCC's -s and
// Cargo's `strip` pass them (-Wl,-s, -Wl,--strip-all, -Wl,--strip-debug),
// still runs: gex binds a module by its names, so gexcc keeps them.

import { join } from 'node:path';
import { FIXTURES, gexcc } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 600 };

export default async function (t) {
  const c = gexcc(t);
  const src = join(FIXTURES, 'ldstrip', 'strip.c');
  const d = c.dir('ldstrip');
  for (const [name, flags] of [['s', ['-s']], ['wl-s', ['-Wl,-s']], ['strip-all', ['-Wl,--strip-all']], ['strip-debug', ['-Wl,--strip-debug']]]) {
    await c.build(['-O2', ...flags, '-o', `${d}/${name}`, src, '-lpthread']);
    const r = await c.exec([`${d}/${name}`, 'x'], { cwd: d });
    t.eq(`${flags.join(' ')}: runs`, `${r.code} ${r.out}`, '0 stripped ok 2 1');
  }
}
