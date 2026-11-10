// TIOCGPTPEER opens a master's slave by descriptor, as openpty
// implementations try before the slave's name (rustix's falls back to the
// name only on ENOSYS or EPERM, so refusing it breaks them).

import { join } from 'node:path';
import { FIXTURES, gexcc } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 600 };

export default async function (t) {
  const c = gexcc(t);
  const d = c.dir('ptypeer');
  await c.build(['-O2', '-o', `${d}/peer`, join(FIXTURES, 'ptypeer', 'peer.c')]);
  const r = await c.exec([`${d}/peer`], { cwd: d });
  t.eq('exit', r.code, 0);
  t.eq('output', r.out.trim(), [
    'peer isatty=1 same=1 ttyname=1 cloexec=1',
    'slave->master up',
    'master->slave down',
    'on slave -1 ENOTTY',
    'on /dev/null -1 ENOTTY',
  ].join('\n'));
}
