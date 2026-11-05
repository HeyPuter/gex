// A snapshot restored many times (test/snapshot/snapshot.mjs): snaptest.c's
// snapshot, restored 10 more times, each checked (the restored threads race
// the process's exit, which a single restore catches only now and then).
import { tail } from '../lib/guest.mjs';
import { snap } from './snapshot.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1200 };

const RESTORES = 10;

export default async function (t) {
  const s = snap(t);
  const { exe, o } = s.snaptest();
  const c = await s.case('snaptest', exe, [], o, { delay: 2000, gap: 3 });
  if (!c) return;
  const want = s.sorted(c.whole.stdout);
  let bad = 0;
  for (let i = 1; i <= RESTORES; i++) {
    const r = await s.run(exe, [], o, { GEX_SNAP_IN: c.snap });
    if (r.code !== 0 || s.sorted(c.before.stdout + r.stdout) !== want) {
      bad++;
      t.say(`restore ${i}: exit ${r.code}: ${tail(r.stderr)}`);
    }
  }
  t.eq(`restored ${RESTORES} more times`, bad, 0);
}
