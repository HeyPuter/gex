// A stored AArch64 module served to a process that has nothing executable
// where the module's builder had code: fixtures/programs/serve-unmapped.c's
// second process maps another library's f where the first process's liba had
// its f, and must run that f, not liba's (the served module's blocks for a
// page that held no code here stayed installed under the new mapping).
import { guest, pageLegs, tail } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 360 };

export default async function (t) {
  const g = guest(t);
  const [leg] = pageLegs(g);
  const main = g.cc('serve-unmapped.c', { glibc: true, libs: ['-ldl'] });
  const liba = g.cc('serve-unmapped.c', { glibc: true, flags: ['-shared', '-fPIC', '-DLIB=3', '-DADD=1'] });
  const libb = g.cc('serve-unmapped.c', { glibc: true, flags: ['-shared', '-fPIC', '-DLIB=5', '-DADD=2'] });
  const r = await g.run(leg.seed, main, [], {
    files: { '/main': main, '/liba.so': liba, '/libb.so': libb, ...g.glibc() }, argv0: '/main', env: leg.env, timeout: 300000,
  });
  t.check('the second process runs its own f', r.code === 0 && r.guest.includes('f(7) = 37 (want 37)'), `exit ${r.code}: ${tail(r.guest, 5)}`);
}
