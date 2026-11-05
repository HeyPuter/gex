// A branch into data is SIGSEGV, natively and on the page's shim: the
// program's handler runs, and without one the whole process dies of it (a
// second thread still running) and its parent's wait says so.
import { native, trim, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 300 };

const want = 'handler: signal 11 at data 1\nhandled: exited 42\nthreaded: killed by signal 11';

export default async function (t) {
  const n = native(t);
  const seed = n.seed();
  const d = n.dir('branch-fault');
  const prog = `${d}/fault`;
  await n.cc(['-O1', '-static', '-o', prog, n.src('branch-fault/fault.c'), '-lpthread']);
  const r = await n.gexc(['run', prog], { timeout: 60000 });
  t.eq('native', trim(r.stdout), want, tail(r.stderr));
  const p = await n.g.run(seed, prog, [], { files: { [prog]: prog }, cwd: d, env: { GEX_SHARED: '1' } });
  t.eq('page', p.guest, want, tail(p.stderr));
  n.clean();
}
