// /proc's threads as Linux has them: tasks.c, built for this machine and run
// here, then built for the guest and run on gex, must print the same (ids
// named, not numbered). The guest runs under gexc and on the page's shim,
// where the process table is the kernel module's and /proc reads it through
// its snapshot.
import { native, exec, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 600 };

const diff = (a, b) => {
  const x = a.split('\n');
  const y = b.split('\n');
  const i = x.findIndex((l, k) => l !== y[k]);
  return i < 0 && x.length === y.length ? '' : `line ${i + 1}: Linux ${JSON.stringify(x[i] ?? '')}, gex ${JSON.stringify(y[i] ?? '')}`;
};

export default async function (t) {
  const n = native(t);
  const seed = n.seed();
  const d = n.dir('proc-tasks');
  await n.hostcc(['-O1', '-pthread', '-o', `${d}/native`, n.src('proc-tasks/tasks.c')]);
  await n.cc(['-O1', '-static', '-pthread', '-o', `${d}/tasks`, n.src('proc-tasks/tasks.c')]);
  const l = await exec([`${d}/native`]);
  if (l.code !== 0) throw new Error(`the Linux run failed: ${tail(l.stdout + l.stderr)}`);
  const linux = l.stdout + l.stderr;
  const g = await n.gexc(['run', `${d}/tasks`], { timeout: 120000 });
  t.check('gexc prints what Linux does', g.stdout === linux, diff(linux, g.stdout));
  const p = await n.g.run(seed, `${d}/tasks`, [], { files: { [`${d}/tasks`]: `${d}/tasks` }, cwd: d, timeout: 300000, env: { GEX_SHARED: '1' } });
  const page = p.stdout.split('\n').filter((x) => !x.startsWith('[')).join('\n');
  t.check('the page prints what Linux does', page === linux, diff(linux, page));
}
