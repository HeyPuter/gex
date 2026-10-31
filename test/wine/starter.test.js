// The page's first Wine process starts the server and has no door, so it
// runs its program again in its own place (a new Worker and memory, the same
// pid): the program's exit status is that process's own, not one passed on
// from a child that ran the program and held its own memory all along.
// Checked from inside the system: a shell starts Wine as its child and reaps
// it; unixpid.exe says which Unix pid it ran in.
import { bootWine, WINE } from '../lib/wine.mjs';

export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['busybox', 'wine64', 'win-progs'], timeout: 600 };

export default async function (t) {
  const w = await bootWine(t, { parts: (sys, fs) => fs.addTar(t.work('bb.tar.zst')) });
  await w.put('unixpid.exe');
  const script = `${WINE} 'C:\\unixpid.exe' & p=$!; wait $p; echo "child $p status $?"`;
  const r = await t.time('starter', () => t.timeout(w.sys.run(['/bin/sh', '-c', script], { env: w.env, cwd: '/pfx/drive_c' }), 300000, 'sh'))
    .catch((e) => ({ code: -1, stdout: '', stderr: String(e.message) }));
  const out = String(r.stdout ?? '').replace(/\r\n/g, '\n');
  const ran = /^unix pid (\d+)$/m.exec(out)?.[1];
  const reaped = /^child (\d+) status (\d+)$/m.exec(out);
  t.check(`the program ran in the shell's child (pid ${ran}, reaped ${reaped?.[1]} with status ${reaped?.[2]})`,
    ran && reaped && ran === reaped[1] && reaped[2] === '42', `stdout ${JSON.stringify(out.slice(-300))}; stderr ${JSON.stringify(String(r.stderr ?? '').trim().split('\n').slice(-4).join(' / '))}`);
  await w.sys.shutdown();
}
