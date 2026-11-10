// A watch belongs to the open file it was added for, not to its descriptor
// number (fixtures/programs/epoll-reuse.c): with the number closed and
// given to a new file, the old watch reported the new file under its token
// (an event loop then ran the wrong source: gpui's ended on a read of an
// empty eventfd), and adding the new file failed with EEXIST.
import { guest, pageLegs, tail } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], timeout: 600 };

const WANT = ['same number 1', 'stale reports 0', 'add again 0 ok', 'reports 1 token 2', 'dup keeps watch 1'].join('\n');

export default async function (t) {
  const g = guest(t);
  const prog = g.cc('epoll-reuse.c', { glibc: true });
  const files = { '/bin/epoll-reuse': prog, ...g.glibc() };
  for (const leg of pageLegs(g)) {
    const r = await g.run(leg.seed, prog, [], { files, argv0: '/bin/epoll-reuse', env: leg.env, single: leg.single });
    t.eq(`epoll-reuse (${leg.label})`, [r.code, r.guest.trim()], [0, WANT], tail(r.stderr, 3));
  }
}
