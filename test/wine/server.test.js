// Wine's server started by the page (`wineserver -f -p`) before any program,
// waited for as Wine's loader waits for one it starts (its ready byte, not
// its socket), then a program on it: wineboot --init's session and the
// program both run with it.
import { bootWine } from '../lib/wine.mjs';

export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 600 };

export default async function (t) {
  const w = await bootWine(t);
  const server = await t.time('server up', () => w.startServer());
  const r = await t.time('hello_args', () => w.run('hello_args.exe', ['a', 'b c'], { timeout: 240000 }))
    .catch((e) => ({ code: -1, stdout: '', stderr: String(e.message) }));
  t.check('hello_args on the page\'s server: exit 42, its output', r.code === 42 && /^hello from a Windows program with 3 args\narg 1: a\narg 2: b c\n$/.test(r.stdout),
    `exit ${r.code}; stdout ${JSON.stringify(r.stdout.slice(-300))}; stderr ${JSON.stringify(r.stderr.trim().split('\n').slice(-4).join(' / '))}`);
  server.kill();
  await w.sys.shutdown();
}
