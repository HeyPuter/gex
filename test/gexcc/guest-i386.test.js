// -fgex-guest-i386: a wasm32 program runs i386 code of its own (a JIT's),
// in its own address space, both ways (fixtures/gexcc/guest-i386/prog.c):
// the program calls it, it calls the program back (which calls it again,
// nested), and the code is rewritten, W^X (mprotect) and in an RWX page, by
// this thread and by another while the other thread's translation is hot.
// Static and linked against libc.so (GEXCC_SHARED=1: gex's loader, dylink),
// natively and on the page's shim.
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { gexcc, FIXTURES } from '../lib/gexcc.mjs';
import { guest, tail } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1500 };

const WANT = 'call 5\ncallback 55\nrewritten 5\nrwx 13 5\nthread-wx 0 wrong of 200\nthread-rwx 0 wrong of 200';

export default async function (t) {
  const c = gexcc(t);
  const g = guest(t);
  const seed = g.pub('seed.x86.shared.wasm');
  const d = c.dir('guest-i386');
  const libc = join(c.sysroot, 'lib/libc.so');
  const kinds = existsSync(libc) ? ['static', 'shared'] : ['static'];
  if (kinds.length === 1) t.say('shared: no libc.so in this gexcc');
  for (const n of kinds) {
    const exe = join(d, n);
    await c.build(['-O2', '-pthread', '-fgex-guest-i386', '-o', exe, join(FIXTURES, 'guest-i386/prog.c')], { shared: n === 'shared' });
    const r = await c.run([exe], { cwd: d, env: n === 'shared' ? { LD_LIBRARY_PATH: c.libdir } : {}, stderr: false });
    t.eq(`${n}: natively`, r.out, WANT);
    const p = await g.run(seed, exe, [], { files: { [`/t/${n}`]: exe, ...(n === 'shared' && { '/usr/lib/wasm32-linux-musl/libc.so': libc }) }, argv0: `./${n}`, flags: ['--cwd', '/t'],
      env: { GEX_LOG: 'off', GEX_SHARED: '1' }, timeout: 600000 });
    const got = p.stdout.split('\n').filter((l) => !/^\[(host|gex|worker)\] |^\[gex-console/.test(l)).join('\n').replace(/\n+$/, '');
    t.eq(`${n}: on the page`, got, WANT, tail(p.stderr));
  }
  c.clean();
}
