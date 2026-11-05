// Each thread's own name and processor time in /proc/<pid>/task/<tid>
// (btop, top -H, a per-thread profile): times.c, built for this machine and
// run here, then built for the guest and run on gex, must print the same.
// Two threads name themselves and spin, one twice the other's time; their
// task stat and comm, read by the process and by a child of it, must carry
// their names and times that grow by what each spun. The guest runs under
// gexc and on the page's shim, where the times are the Workers' and the
// process table is the kernel module's.
import { native, exec, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 600 };

export default async function (t) {
  const n = native(t);
  const seed = n.seed();
  const d = n.dir('thread-times');
  await n.hostcc(['-O1', '-pthread', '-o', `${d}/native`, n.src('thread-times/times.c')]);
  await n.cc(['-O1', '-static', '-pthread', '-o', `${d}/times`, n.src('thread-times/times.c')]);
  const l = await exec([`${d}/native`]);
  if (l.code !== 0) throw new Error(`the Linux run failed: ${tail(l.stdout + l.stderr)}`);
  const linux = l.stdout.replace('called native', 'called times');
  if (linux.includes('FAIL')) throw new Error(`the check does not hold on Linux: ${tail(linux)}`);
  const g = await n.gexc(['run', `${d}/times`], { timeout: 120000 });
  t.check('gexc prints what Linux does', g.stdout === linux, tail(g.stdout + g.stderr));
  const p = await n.g.run(seed, `${d}/times`, [], { files: { [`${d}/times`]: `${d}/times` }, cwd: d, timeout: 300000, env: { GEX_SHARED: '1' } });
  const page = p.stdout.split('\n').filter((x) => !x.startsWith('[')).join('\n');
  t.check('the page prints what Linux does', page === linux, tail(page + p.stderr));
}
