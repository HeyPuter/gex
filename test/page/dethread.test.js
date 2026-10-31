// An exec from a second thread (Linux's de_thread, shims/browser/src/lib.rs
// `exec_waits`): fixtures/programs/exec-dethread.c execs from a new thread
// while the first thread joins it, spins in translated code, blocks reading
// a pipe, or waits on a futex, each 20 times (2 in the single-thread build,
// whose spinning thread holds the one realm for long slices). Every new
// program must start and exit 0; with the old image replaced under a
// running thread, that thread ran the old code over the new one.
import { guest, pageLegs, tail } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], timeout: 1500 };

export default async function (t) {
  const g = guest(t);
  const legs = pageLegs(g, { sp: true });
  const prog = g.cc('exec-dethread.c', { glibc: true, flags: ['-pthread'] });
  const files = { '/bin/exec-dethread': prog, ...g.glibc() };
  for (const leg of legs) {
    const rounds = leg.single ? 2 : 20;
    const r = await g.run(leg.seed, prog, [rounds], { files, argv0: '/bin/exec-dethread', env: leg.env, single: leg.single, timeout: 600000 });
    const why = r.stderr.split('\n').filter((l) => /nothing to translate|went ahead after/.test(l)).slice(0, 3).join(' / ');
    t.eq(`exec-dethread, ${rounds} rounds of 4 cases (${leg.label})`, [r.code, r.guest], [0, `exec-dethread ok ${rounds}`], why || tail(r.stderr, 3));
  }
}
