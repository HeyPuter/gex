// node's child_process in the browser runtime under Node (run-node.mjs: the
// page's shim and glue), Debian arm64 node translated
// (fixtures/gexcc/node-child/cp.js): the socketpair semantics libuv relies
// on, and nvim's system() running node (where the tree has nvim).
// GEX_TEST_NODE_ROOT: a Debian arm64 tree with usr/bin/node and its
// libraries (usr/bin/nvim optional); GEX_TEST_NODE_SH: Debian arm64 dash.
// Correctness only.
import { copyFileSync, existsSync } from 'node:fs';
import { join } from 'node:path';
import { FIXTURES } from '../lib/gexcc.mjs';
import { native, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 3600 };

export default async function (t) {
  const root = process.env.GEX_TEST_NODE_ROOT || t.skip('GEX_TEST_NODE_ROOT: a Debian arm64 tree with usr/bin/node');
  const sh = process.env.GEX_TEST_NODE_SH || t.skip('GEX_TEST_NODE_SH: Debian arm64 dash');
  const n = native(t);
  const seed = n.g.pub('seed{infix}.shared.wasm');
  const tmp = n.dir('node-child');
  copyFileSync(join(FIXTURES, 'node-child/cp.js'), join(tmp, 'cp.js'));
  const flags = ['--dir', `/=${root}`, '--dir', `/lib=${join(root, 'usr/lib')}`, '--file', `/bin/sh=${sh}`, '--file', `/usr/bin/sh=${sh}`, '--dir', `/tmp=${tmp}`,
    '--env', 'HOME=/tmp', '--env', 'PATH=/usr/bin:/bin'];
  const run = (prog, args) => n.g.run(seed, join(root, prog), args, { argv0: prog, flags, env: { GEX_SHARED: '1' }, timeout: 1500000, merge: true });
  const r = await run('/usr/bin/node', ['/tmp/cp.js']);
  const lines = r.all.split('\n');
  for (const l of lines.filter((x) => /^(ok|FAIL) /.test(x))) t.check(l.replace(/^(ok|FAIL) /, '').split(':')[0], l.startsWith('ok '), l);
  t.check('cp.js finished', lines.includes('END'), tail(r.all));
  if (existsSync(join(root, 'usr/bin/nvim'))) {
    const v = await run('/usr/bin/nvim', ['--headless', '-c', 'lua io.stdout:write("system: "..vim.fn.system({"node","-p","6*7"}))', '-c', 'q']);
    t.check('nvim system() runs node', /^system: 42/m.test(v.all), tail(v.all.split('\n').filter((l) => !l.startsWith('[')).join('\n')));
  } else t.say('nvim: left out (no usr/bin/nvim in the tree)');
  n.clean();
}
