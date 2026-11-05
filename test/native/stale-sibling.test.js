// A block whose code runs onto a page translated again since is reached by a
// direct branch from a block of its own translation whose page did not
// change: that edge is stale for good, and every call through it dispatches
// on to the newer translation from inside the guest, not in a trip out to
// JavaScript and back each time. Natively, then on the page's shim in both
// numberings: dense (the default) and classic (GEX_DENSE=0, which a closure
// image and ?dense=0 run). GEX_JIT_DIAG says how often stale blocks
// dispatched on (thousands of times here), how often one was resumed from
// JavaScript from the second time on (it must not be), and how often the
// stale block was translated again (from 4 times on; it must not be: classic
// numbering once translated it again on every sibling jump, a new module
// each time, without end).
import { native, count, trim, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 400 };

export default async function (t) {
  const n = native(t);
  const seed = n.seed();
  const d = n.dir('stale-sibling');
  const prog = `${d}/sibling`;
  await n.cc(['-O1', '-static', '-o', prog, n.src('stale-sibling/sibling.c')]);
  const r = await n.gexc(['run', prog], { timeout: 60000 });
  t.eq('native', trim(r.stdout), 'sum 50000', tail(r.stderr));
  for (const [flavour, dense] of [['dense', '1'], ['classic', '0']]) {
    const p = await n.g.run(seed, prog, [], { files: { [prog]: prog }, cwd: d, timeout: 120000,
      env: { GEX_DENSE: dense, GEX_JIT_DIAG: '1', GEX_SHARED: '1' } });
    const again = count(p.stderr, /resumed from JavaScript \d+ times/);
    const onward = count(p.stderr, /dispatched on to a newer translation/);
    const retrans = p.stderr.match(/translated again (\d+) times/g)?.at(-1);
    t.eq(`page ${flavour}`, p.guest, 'sum 50000', tail(p.stderr));
    t.check(`page ${flavour}: dispatched on (${onward} lines), never resumed again (${again}), never translated again (${retrans ?? 'under 4 times'})`,
      again === 0 && onward >= 1 && !retrans, p.stderr.split('\n').filter((l) => /resumed from JavaScript|translated again/.test(l)).slice(-3).join(' / '));
  }
  n.clean();
}
