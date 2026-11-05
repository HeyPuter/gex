// The translation cache across loads in the browser runtime under Node
// (run-node.mjs, GEX_XCACHE; fixtures/gexcc/xcache): a run restores what
// the last one saved, and a library replaced at the same path (between
// runs, or during one after the restore and before any process uses it,
// swap.c) is not served from the cache: its new code runs. Two processes'
// saves at once (pair.c: the child's still being written when pid 1 exits,
// GEX_XCACHE_DELAY_MS) both land: the child's whether or not pid 1 has
// anything to add, and pid 1's after the child's when it has. A lazy file
// (a site's lazy.json files) has the same time on every load, so its keys
// are restored rather than dropped as changed. Counts and output only.
import { copyFileSync, existsSync, mkdirSync, renameSync, rmSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { FIXTURES } from '../lib/gexcc.mjs';
import { tail } from '../lib/native.mjs';
import { pageA64 } from './page-a64.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 1500 };

const expect = (v) => Math.floor(1000 / 7) * 21 * v + (1000 % 7) * ((1000 % 7) - 1) / 2 * v + v;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export default async function (t) {
  const p = pageA64(t);
  const d = p.root('xcache');
  const root = join(d, 'root');
  const src = (f) => join(FIXTURES, 'xcache', f);
  const lib = (v, out = join(root, 'lib/libt.so')) => p.n.cc(['-O1', '-shared', '-fPIC', `-DVALUE=${v}`, '-o', out, src('libt.c')]);
  await lib(1);
  await p.n.cc(['-O1', '-o', join(root, 'bin/t'), src('main.c'), `-L${join(root, 'lib')}`, '-lt']);
  for (const b of ['swap', 'pair']) await p.n.cc(['-O1', '-o', join(root, `bin/${b}`), src(`${b}.c`)]);
  const xc = (dir) => ({ GEX_XCACHE: 'gate', GEX_XCACHE_DIR: dir, GEX_XCACHE_MIN: '0' });
  mkdirSync(join(d, 'xc'));
  const run = (prog = '/bin/t', env = {}, r = root, flags = []) => p.run(r, prog, { env: { ...xc(join(d, 'xc')), ...env }, flags }).then((x) => x.all);
  const check = (name, v, re, out) => {
    const lines = out.split('\n');
    if (!lines.includes(`value ${v}`)) return t.check(name, false, `wanted value ${v}, got ${lines.filter((l) => l.startsWith('value')).join(' ') || tail(out)}`);
    t.check(name, re.test(out), `wanted ${re} in: ${lines.filter((l) => l.includes('xcache')).join(' / ')}`);
  };
  check('cold run saves', expect(1), /xcache gate: saved/, await run());
  const b = await run();
  check('warm run restores', expect(1), /restored [1-9][0-9]* modules/, b);
  t.check('warm run: no file it did not touch counted as changed', !b.includes('changed since the save'));
  // (file times are kept to the second: a second between the builds)
  await sleep(1100);
  await lib(2);
  check('a replaced library is not served stale', expect(2), /libt\.so changed since the save/, await run());
  check('and the next run restores the new one', expect(2), /restored [1-9][0-9]* modules/, await run());
  await sleep(1100);
  await lib(3, join(root, 'lib/libt.new'));
  // GEX_JIT_DIAG: this run also says when a stale block of a module it did
  // not translate (restored here) has the id of one of its own, and that the
  // block was judged by its own module's range, never the one the id names
  // (ids count from 1 in every run; taking the id for the module sent the
  // stale block back to JavaScript for ever, and the run hung before exec)
  const e = await run('/bin/swap', { GEX_JIT_DIAG: '1' });
  check('a library replaced after the restore is not served stale', expect(3), /libt\.so changed since the save/, e);
  check('a restored stale block whose id one of the run\'s own has is not sent elsewhere', expect(3), /whose id [0-9]+ one of its own has too; (not taken for elsewhere|judged by its own range)/, e);
  // the guest's rename stays in that run's filesystem (run-node copies
  // --dir): the next run has the old libt.so again, which the cache (saved
  // with the new one) must not serve either
  check('and the next run, back on the old one, is not served the new', expect(2), /libt\.so changed since the save/, await run());

  const xcp = join(d, 'xcp');
  mkdirSync(xcp);
  for (const late of [false, true]) {
    rmSync(join(xcp, 'gate'), { force: true });
    const o = await p.run(root, '/bin/pair', { args: late ? ['late'] : [], env: { ...xc(xcp), GEX_XCACHE_DELAY_MS: '3000' } }).then((x) => x.all);
    const lines = o.split('\n');
    const child = lines.filter((l) => /xcache gate: saved .*, pid ([2-9]|[1-9][0-9]+)$/.test(l)).length;
    const first = lines.filter((l) => /xcache gate: saved .*, pid 1$/.test(l)).length;
    t.check(`overlapping saves${late ? ' (pid 1 adds)' : ''}: the child saved${late ? ', then pid 1' : ''}`, child === 1 && (!late || first === 1) && existsSync(join(xcp, 'gate')) && statSync(join(xcp, 'gate')).size > 0,
      `child ${child}, pid 1 ${first}: ${lines.filter((l) => l.includes('xcache')).join(' / ')}`);
    check(`  and the next run restores it${late ? ' (pid 1 adds)' : ''}`, expect(2), /restored [1-9][0-9]* modules/, await p.run(root, '/bin/t', { env: xc(xcp) }).then((x) => x.all));
  }

  // a library read lazily (--lazy) comes from the cache on the next run
  const lroot = join(d, 'lroot');
  for (const s of ['lib', 'bin']) mkdirSync(join(lroot, s), { recursive: true });
  mkdirSync(join(d, 'lz'));
  mkdirSync(join(d, 'xcl'));
  const ld = Object.keys(p.n.ldso)[0];
  copyFileSync(join(root, ld), join(lroot, ld));
  copyFileSync(join(root, 'bin/t'), join(lroot, 'bin/t'));
  await lib(4, join(d, 'lz.so'));
  renameSync(join(d, 'lz.so'), join(d, 'lz/libt.so'));
  const runl = () => p.run(lroot, '/bin/t', { env: xc(join(d, 'xcl')), flags: ['--lazy', `/lib/libt.so=${join(d, 'lz/libt.so')}`] }).then((x) => x.all);
  check('a cold run with a lazy library saves', expect(4), /xcache gate: saved/, await runl());
  await sleep(1100);
  const h = await runl();
  check('the next run restores it', expect(4), /restored [1-9][0-9]* modules/, h);
  t.check('the lazy library is not counted as changed', !h.includes('changed since the save'), h.split('\n').find((l) => l.includes('changed since')));
  p.n.clean();
}
