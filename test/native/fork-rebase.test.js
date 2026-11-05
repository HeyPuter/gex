// A forked child adopting its parent's translations keeps the directory it
// inherited: where the parent moved a page to another base after translating
// it, the translation's older base is not written back (GEX_TEST_FORK_REBASE
// moves the first adopted module's pages, as such a parent leaves them;
// GEX_JIT_DIAG says each page given a second base). On the page's shim.
import { native, count, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 300 };

export default async function (t) {
  const n = native(t);
  const seed = n.seed();
  const d = n.dir('fork-rebase');
  const prog = `${d}/fork`;
  await n.cc(['-O1', '-static', '-pthread', '-o', prog, n.src('fork-rebase/fork.c')]);
  const r = await n.g.run(seed, prog, [], { files: { [prog]: prog }, cwd: d, timeout: 120000,
    env: { GEX_ADOPT_AFTER: '0', GEX_TEST_FORK_REBASE: '1', GEX_JIT_DIAG: '1', GEX_SHARED: '1' } });
  t.eq('the child and parent', r.guest, 'child 299995\nparent 299995 2997, child ok', tail(r.stderr));
  t.check('pages moved', count(r.stderr, /by test-fork-rebase/) >= 1, tail(r.stderr));
  t.check('none moved back', count(r.stderr, /re-based .* by register-module/) === 0, r.stderr.split('\n').filter((l) => /re-based/.test(l)).slice(-3).join(' / '));
  n.clean();
}
