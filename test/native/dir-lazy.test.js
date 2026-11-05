// run-node's --dir bundles small files and makes large ones lazy (read as
// the guest reads them, as a page's rest bundle is): a lazily backed file
// must behave as a bundled one: exec'd, mapped, written into, renamed over
// (lazyfile.c). Both ways: lazy (the default) and GEX_DIR_EAGER=1. The page
// runtime only: there are no lazy files natively.
import { copyFileSync, readFileSync, statSync, writeFileSync, mkdirSync } from 'node:fs';
import { native, tail, ROOT } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 400 };

export default async function (t) {
  const n = native(t);
  const seed = n.seed();
  // On disk: under /dev/shm the guest would see its own /dev.
  const d = n.dir('dir-lazy');
  await n.cc(['-O1', '-static', '-o', `${d}/lazyfile`, n.src('dir-lazy/lazyfile.c')]);
  const big = Buffer.alloc(3 << 20);
  for (let i = 0; i < big.length; i++) big[i] = (i * 131 + (i >> 12)) & 255;
  writeFileSync(`${d}/big.bin`, big);
  writeFileSync(`${d}/over.bin`, Buffer.alloc(100000));
  for (const mode of ['lazy', 'eager']) {
    const m = `${d}/d-${mode}`;
    mkdirSync(m);
    copyFileSync(`${d}/lazyfile`, `${m}/child`);
    copyFileSync(`${d}/big.bin`, `${m}/big.bin`);
    copyFileSync(`${d}/over.bin`, `${m}/over.bin`);
    if (statSync(`${m}/child`).size < 65536) throw new Error('child is under 64 KiB, so not lazy');
    const r = await n.g.run(seed, `${d}/lazyfile`, ['/d'], {
      flags: ['--dir', `/d=${m}`], files: { '/t/lazyfile': `${d}/lazyfile` }, argv0: '/t/lazyfile', cwd: ROOT,
      env: { GEX_DIR_EAGER: mode === 'eager' ? '1' : '0', GEX_SHARED: '1' },
    });
    const oks = r.stdout.split('\n').filter((l) => l.startsWith('ok')).length;
    t.check(`${mode} (${oks} checks)`, r.code === 0 && /^0 failed/m.test(r.stdout),
      `exit ${r.code}: ${r.stdout.split('\n').filter((l) => l && !l.startsWith('ok')).join(' / ')} / ${tail(r.stderr, 3)}`);
    // the host's file stays as it was
    t.check(`${mode}: the guest's write did not reach the host file`, readFileSync(`${m}/big.bin`).equals(big));
  }
  n.clean();
}
