// GEX_TRAP_STACK=1: a guest trap's engine stack on stderr, in the page's
// runtime under Node, and nothing of it without the switch. The program is
// fixtures/gexcc/programs/faultsig.c's `trap` case (__builtin_trap, SIGILL).

import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { FIXTURES, gexcc } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 600 };

const ROOT = join(import.meta.dirname, '..', '..');

export default async function (t) {
  const c = gexcc(t);
  const x86 = t.arch === 'x86';
  const seed = join(ROOT, 'page/public/gex', `seed${x86 ? '.x86' : ''}.shared.wasm`);
  if (!existsSync(seed)) t.skip(`no ${seed} (make assets)`);
  const d = c.dir('trapstack');
  await c.build(['-O1', '-w', '-o', `${d}/faultsig`, join(FIXTURES, 'programs', 'faultsig.c')]);
  const run = (on) => c.run([process.execPath, join(ROOT, 'hosts/browser/js/run-node.mjs'),
    '--file', `/t/faultsig=${d}/faultsig`, '--cwd', '/t', '--argv0', './faultsig', seed, `${d}/faultsig`, 'trap'],
  { cwd: d, env: { GEX_ARCH: x86 ? 'x86' : 'aarch64', GEX_SHARED: '1', ...(on ? { GEX_TRAP_STACK: '1' } : {}) } });
  const on = await run(true);
  const line = on.out.split('\n').find((l) => l.includes('[trapstack]')) ?? '';
  t.check('trapstack: the trap and its stack', /unreachable/i.test(line) && /wasm-function|wasm:\/\//.test(line), on.out.split('\n').slice(-6).join(' / '));
  t.check('trapstack: the process died of SIGILL', on.code === 128 + 4 || on.code === -4 || on.code === 4, `exit ${on.code}`);
  const off = await run(false);
  t.check('off: no stack', !off.out.includes('[trapstack]'), off.out.split('\n').slice(-3).join(' / '));
  c.clean();
}
