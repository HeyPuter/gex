// The page's main thread lets go of a fork child's record once the child's
// Worker has it, though the child runs on: the record names every module
// the parent had (its own and its parents'), and held for each child's life
// they came to some twenty thousand modules on a desktop's page.
// (GEX_TEST_FORK_RECORDS: the host says, half a second after the hand-over
// and a collection, whether the record is still reachable.)
//
// And a page leg's view of a game's memory (peek.c): `?memories=1` (here
// GEX_MEMORIES=1) registers a threaded process's memory for the harness, and
// with the fork record let go that registry is all that must keep it. Read
// after a collection twice a second (GEX_TEST_MEMORIES), the child's
// counter must be there and climbing while it runs, and let go once it has
// exited; without the switch nothing is registered.
import { native, exec, count, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 600 };

export default async function (t) {
  const n = native(t);
  const seed = n.seed();
  const d = n.dir('spawn-release');
  await n.cc(['-O1', '-static', '-o', `${d}/fork`, n.src('spawn-release/fork.c')]);
  const r = await n.g.run(seed, `${d}/fork`, [], { files: { [`${d}/fork`]: `${d}/fork` }, cwd: d, node: ['--expose-gc'],
    env: { GEX_TEST_FORK_RECORDS: '1', GEX_SHARED: '1' } });
  const released = count(r.stderr, /fork record released while it runs/);
  const kept = count(r.stderr, /fork record kept while it runs/);
  t.eq('the child and parent', r.guest, 'child 2000\nparent 400000, child ok', tail(r.stderr));
  t.check(`the fork record released while the child runs (${released} released, ${kept} kept)`, released >= 1 && kept === 0,
    r.stderr.split('\n').filter((l) => /\[test\]/.test(l)).slice(-3).join(' / '));

  await n.cc(['-O1', '-static', '-no-pie', '-o', `${d}/peek`, n.src('spawn-release/peek.c'), '-lpthread']);
  const nm = (await exec([n.gccPath.replace(/gcc$/, 'nm'), `${d}/peek`])).stdout;
  const at = `0x${/^([0-9a-f]+) . counter$/m.exec(nm)?.[1]}`;
  const peek = (env) => n.g.run(seed, `${d}/peek`, [], { files: { [`${d}/peek`]: `${d}/peek` }, cwd: d, node: ['--expose-gc'],
    env: { GEX_TEST_MEMORIES: at, GEX_SHARED: '1', ...env } });
  const on = await peek({ GEX_MEMORIES: '1' });
  const words = [...on.stderr.matchAll(/\[test\] memories: pid.* word (\d+)/g)].map((m) => Number(m[1]));
  const climbs = words.filter((w, i) => i > 0 && w > words[i - 1]).length;
  const left = /\[test\] memories at exit.*: (.*)$/m.exec(on.stderr)?.[1];
  t.eq('memories: the child counted', on.guest, 'child counted 200\nchild ok', tail(on.stderr));
  t.check(`memories: the counter read ${climbs} times higher while the child ran (at least 3), then let go (${left ?? '?'} held)`, climbs >= 3 && left === '0');
  const off = await peek({});
  t.check('memories off: nothing registered', !/\[test\] memories: pid/.test(off.stderr));
  n.clean();
}
