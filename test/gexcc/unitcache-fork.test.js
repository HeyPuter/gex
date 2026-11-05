// The page's unit cache and the dense base, across a fork (run-node: the
// page's shim and glue under Node; fixtures/gexcc/unitcache-fork/main.c):
// process B holds Modules process A compiled, whose gex.dense section names
// A's base (A and B load their libraries in opposite orders); a child B forks
// adopts them and must number them at B's base. The results must match a
// native run of the same C on the host, and GEX_DENSE_DEBUG must log adopts
// of a Module carrying another base (so the case was reached).
//
// With that fix reverted the results still match: adopt numbered its slots
// and the Module's blocks at the same (foreign) base, which is consistent
// within the child, and in this run the child instantiates none of those
// Modules. Only the log check fails then. A wrong result needs a foreign
// range that overlaps one of the child's own and both instantiated, which
// this test does not reach.
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { exec, tail } from '../lib/native.mjs';
import { FIXTURES } from '../lib/gexcc.mjs';
import { pageA64 } from './page-a64.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 600 };

/** libbar.c: two families of N small functions, bar_a_* (run only by
 *  process A) and bar_b_* (run only by B), each numbering many dense blocks. */
function libbar(n = 400) {
  const l = [];
  for (const f of ['a', 'b']) {
    for (let i = 0; i < n; i++) l.push(`int bar_${f}_${i}(int x) { int s = ${i}; for (int j = 0; j < x; j++) s = s * ${i % 13 + 2} + j${f === 'b' ? ' + 1' : ''}; return s; }`);
    l.push(`int bar_${f}_all(int x) { int s = 0;`, ...Array.from({ length: n }, (_, i) => `  s ^= bar_${f}_${i}(x);`), '  return s; }');
  }
  return `${l.join('\n')}\n`;
}

export default async function (t) {
  const p = pageA64(t);
  const d = p.root('unitcache-fork');
  const src = (f) => join(FIXTURES, 'unitcache-fork', f);
  writeFileSync(join(d, 'libbar.c'), libbar());
  await p.n.cc(['-O1', '-shared', '-fPIC', '-o', join(d, 'root/lib/libfoo.so'), src('libfoo.c')]);
  await p.n.cc(['-O1', '-shared', '-fPIC', '-o', join(d, 'root/lib/libbar.so'), join(d, 'libbar.c')]);
  await p.n.cc(['-O1', '-o', join(d, 'root/bin/t'), src('main.c')]);
  // the expected values: the same C on the host
  mkdirSync(join(d, 'nat'));
  await p.n.hostcc(['-O1', '-shared', '-fPIC', '-o', join(d, 'nat/libfoo.so'), src('libfoo.c')]);
  await p.n.hostcc(['-O1', '-shared', '-fPIC', '-w', '-o', join(d, 'nat/libbar.so'), join(d, 'libbar.c')]);
  await p.n.hostcc(['-O1', '-w', '-o', join(d, 'native'), src('main.c'), '-ldl']);
  const want = (await Promise.all(['a', 'b'].map((w) => exec([join(d, 'native'), w], { cwd: d, env: { ...p.n.env, LD_LIBRARY_PATH: join(d, 'nat') } })))).map((r) => r.stdout).join('');
  // GEX_ADOPT_AFTER=0: the child adopts what it inherits at the fork, so the
  // Modules from the unit cache meet its numbering for certain
  const r = await p.run(join(d, 'root'), '/bin/t', { env: { GEX_ADOPT_AFTER: '0', GEX_DENSE_DEBUG: '1' } });
  const out = r.all.split('\n');
  for (const l of ['a: bar=', 'a: f=', 'b: bar=', 'b: f=', 'child: f=', 'child: bar=']) {
    const w = want.split('\n').find((x) => x.startsWith(l));
    t.check(`${l.trim()} as natively (${w})`, w !== undefined && out.includes(w), out.find((x) => x.startsWith(l)) ?? 'no line');
  }
  t.check('b: child status 0', out.includes('b: child status 0'), out.find((x) => x.includes('child status')) ?? tail(r.all));
  t.check('launcher: a=0 b=0', out.includes('launcher: a=0 b=0'), out.find((x) => x.startsWith('launcher')) ?? tail(r.all));
  const adopts = out.filter((x) => x.includes('adopt: a module compiled elsewhere')).length;
  t.check(`adopts of a module compiled at another base (${adopts})`, adopts > 0, 'the case was not reached');
  p.n.clean();
}
