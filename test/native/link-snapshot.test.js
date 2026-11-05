// The page's linked-image store (GEX_LINK): an x86-64 glibc program that
// execs itself once. With the store on, the second image starts from what
// the first one's ld.so produced, so ld.so opens nothing after the execve;
// with it off (the control) it opens its libraries again. The image goes
// through the global kernel's snapshot store (k_snap_store/k_snap_get).
import { native, exec } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 600 };

export default async function (t) {
  const n = native(t);
  const seed = n.seed();
  n.g.pub('gex_kernel.x86.wasm');
  const glibc = n.g.glibc();
  const d = n.dir('link-snapshot');
  const b = await exec([n.g.tool('gcc'), '-O1', '-o', `${d}/le`, n.src('link-snapshot/le.c')]);
  if (b.code !== 0) throw new Error(`gcc: ${b.stderr.trim()}`);
  for (const link of ['0', '1']) {
    const r = await n.g.run(seed, `${d}/le`, [], { files: { '/bin/le': `${d}/le`, ...glibc }, argv0: '/bin/le', timeout: 180000,
      env: { GEX_LINK: link, GEX_TRACE: '1', GEX_SHARED: '1' } });
    const out = r.stdout.split('\n').filter((l) => l && !l.startsWith('[')).join('\n');
    const at = r.stderr.indexOf('execve');
    const opens = at < 0 ? 0 : r.stderr.slice(r.stderr.lastIndexOf('\n', at) + 1).split('\n').filter((l) => l.includes('openat')).length;
    t.check(`link=${link}: exits 0, "again ran"`, r.code === 0 && out === 'again ran', `exit ${r.code}, output: ${out}`);
    if (link === '0') t.check(`control: ld.so opens its libraries again after the execve (${opens} opens)`, opens > 0, 'it opened nothing, so the check proves nothing');
    else t.check(`the exec'd image does not link again (${opens} opens after the execve)`, opens === 0);
  }
  n.clean();
}
