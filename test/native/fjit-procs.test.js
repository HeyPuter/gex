// The function JIT keeps what it learns per guest process: a parent and the
// image its child execs share addresses, not code (procs.c). Output under
// gexc, with and without --fjit, must match native.
import { native, exec, trim } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

export default async function (t) {
  const n = native(t);
  const d = n.dir('fjit-procs');
  const zig = n.g.tool('zig');
  for (const [out, more] of [['procs', []], ['procs-child', ['-DCHILD']]]) {
    const b = await exec([zig, 'cc', '-target', 'x86_64-linux-musl', '-O1', '-static', ...more, '-o', `${d}/${out}`, n.src('fjit-procs/procs.c')]);
    if (b.code !== 0) throw new Error(`zig cc: ${b.stderr.trim()}`);
  }
  const sym = async (file, name) => (await exec(['nm', file])).stdout.split('\n').map((l) => l.split(' ')).find((f) => f[2] === name)?.[0];
  const pa = await sym(`${d}/procs`, 'probe');
  const ca = await sym(`${d}/procs-child`, '__memset_gexprobe');
  if (!pa || pa !== ca) throw new Error(`probe at ${pa} in the parent but __memset_gexprobe at ${ca} in the child (the test needs them equal)`);
  const want = trim((await exec([`${d}/procs`, `${d}/procs-child`])).stdout);
  for (const flags of [[], ['--fjit']]) {
    const r = await n.gexc(['run', ...flags, `${d}/procs`, `${d}/procs-child`]);
    t.eq(`${flags[0] ?? 't1'}: as native`, trim(r.stdout + r.stderr), want);
  }
  n.clean();
}
