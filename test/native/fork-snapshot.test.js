// A fork's child gets the parent's memory as it was at one moment, as Linux
// copies it, while the parent's other threads run on (main.c): threads write
// B then A, far apart in memory, and the child must see them as some moment
// left them. Copied a region at a time while those threads ran, the child's
// memory had A from one moment and B from a later one, every fork; glibc's
// child walked its stack lists as no thread ever left them and crashed. On
// the page's shim; FORKS (default 20) forks a case.
import { native, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 1200 };

export default async function (t) {
  const n = native(t);
  const seed = n.seed();
  const d = n.dir('fork-snapshot');
  await n.cc(['-O1', '-o', `${d}/main`, n.src('fork-snapshot/main.c'), '-lpthread']);
  const forks = process.env.FORKS || '20';
  // THREADS writers, CALLS (the writers but the first make syscalls), CHURN
  // (a thread making and joining threads); the first case's one writer never
  // makes a syscall. The single-thread build (fibers, JSPI) too, last: its
  // threads never run during a copy, and its memory has no wait.
  for (const [threads, calls, churn, single] of [[1, 0, 0, false], [4, 0, 0, false], [4, 1, 1, false], [4, 1, 1, true]]) {
    const r = await n.g.run(seed, `${d}/main`, [forks, threads, calls, churn], {
      files: { ...n.ldso, [`${d}/main`]: `${d}/main` }, cwd: d, timeout: 300000, single, env: { GEX_SHARED: '1' } });
    const what = `${forks} forks beside ${threads} writing thread(s)${calls ? ', all but one making calls' : ''}${churn ? ', threads made and joined' : ''}${single ? ', single-thread build' : ''}`;
    const ms = /^fork ms.*$/m.exec(r.stdout)?.[0] ?? '';
    t.check(`${what}${ms ? `; ${ms}` : ''}`, r.code === 0 && new RegExp(`^forks ${forks} torn 0$`, 'm').test(r.stdout),
      `exit ${r.code}: ${/^forks.*$/m.exec(r.stdout)?.[0] ?? ''} / ${r.stdout.split('\n').filter((l) => l.startsWith('child')).slice(0, 3).join(' / ')} / ${r.stderr.split('\n').filter((l) => /torn|panic/.test(l)).slice(0, 3).join(' / ') || tail(r.stderr, 3)}`);
  }
  n.clean();
}
