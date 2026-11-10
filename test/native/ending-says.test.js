// What gex says at its default level from a Worker that is ending reaches
// the host: each of 20 children falls short of the lowered memory and
// aborts (fixtures/native/ending-says/ending.c), and its own Worker says,
// as it ends, that the program needs a 64-bit memory. A Worker's console
// was a stream its parent drained, which terminate() dropped, so such a
// line was often lost.
import { native } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 300 };

const N = 20;

export default async function (t) {
  const n = native(t);
  const d = n.dir('ending');
  await n.cc(['-O1', '-static', '-o', `${d}/ending`, n.src('ending-says/ending.c')]);
  const files = { '/g/ending': `${d}/ending` };
  // (a copy each: a record on one would give the next a 64-bit memory)
  for (let i = 0; i < N; i++) files[`/g/c${i}`] = `${d}/ending`;
  const r = await n.g.run(n.seed(), `${d}/ending`, [String(N)], { files, argv0: '/g/ending', env: { GEX_SHARED: '1' }, timeout: 240000 });
  const all = r.stdout + r.stderr;
  t.check('it ran', /ending done/.test(r.stdout), all.slice(-800));
  const said = new Set(all.match(/\/g\/c\d+ needs 64-bit memory/g) ?? []).size;
  t.eq('every ending process says it', said, N, all.slice(-800));
  n.clean();
}
