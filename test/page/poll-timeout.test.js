// Timed waits that only their deadline ends (fixtures/programs/
// poll-timeout.c): poll, ppoll, select and epoll_wait on a pipe nobody
// writes, 100 ms each, in a main thread, a second thread and a forked child.
// Such a wait gives the stack back to the run loop by a trap, and the trap
// left the syscall's clock scope (`NowScope`) open: the clock stayed at the
// call's own reading and the deadline never came. On x86 also in the
// single-thread build.
import { guest, pageLegs, tail } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], timeout: 300 };

export default async function (t) {
  const g = guest(t);
  const legs = pageLegs(g, { sp: true });
  const prog = g.arch === 'x86'
    ? g.cc('poll-timeout.c', { glibc: true, flags: ['-static', '-pthread'] })
    : g.cc('poll-timeout.c', { imageBase: false, opt: '-O1', flags: ['-pthread'] });
  for (const leg of legs) {
    const r = await g.run(leg.seed, prog, [], { env: leg.env, single: leg.single });
    t.eq(`poll-timeout (${leg.label})`, [r.code, r.guest], [0, 'poll-timeout ok'], tail(r.stderr, 3));
  }
}
