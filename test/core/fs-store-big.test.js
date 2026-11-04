// An OPFSFilesystem's store past the sizes 32-bit offsets reach: a file of
// 4.25 GiB written through a 64 MiB cache, summed as it is written and read
// back (whole, and from past 4 GiB). In the profile's own context
// (`meta.profile`), whose storage is on disk as a user's is (a fresh
// context's stops a file near 2 GiB: fs-store-full).
import { System } from '../../page/runtime/src/index.js';
import { OPFSFilesystem } from '../../page/runtime/src/fs.js';

export const meta = { hosts: ['browser'], arches: ['a64'], images: ['slim'], timeout: 1800, profile: true };

const ENV = { PATH: '/usr/sbin:/usr/bin:/sbin:/bin', HOME: '/root' };
const MB = 4352; // 4.25 GiB

async function sh(t, sys, cmd, ms = 900000) {
  const r = await t.timeout(sys.run(['sh', '-c', cmd], { env: ENV }), ms, cmd.slice(0, 40));
  return { code: r.code, out: (r.stdout + r.stderr).trim() };
}

export default async function (t) {
  const { eq, check, timeout } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const name = `gex-test-big-${t.arch}`;
  await OPFSFilesystem.erase(name);
  const fs = new OPFSFilesystem(sys, { name, cache: 64 });
  fs.addTar(t.work('slim/debian-slim.tar.zst'));
  await timeout(sys.boot(), 60000, 'boot');
  const w = await sh(t, sys, `dd if=/dev/urandom bs=1048576 count=${MB} 2>/dev/null | tee /root/big | md5sum | cut -d' ' -f1`);
  eq('4.25 GiB written through a 64 MiB cache', w.code, 0);
  const r = await sh(t, sys, 'md5sum /root/big | cut -d" " -f1; wc -c < /root/big');
  eq('read back whole: the same sum and size', [r.code, r.out], [0, `${w.out}\n${MB * 1048576}`]);
  // The last 64 MiB (past 4 GiB) read straight from where they are, against
  // the same bytes read through a pipe from the start.
  const tail = await sh(t, sys, `dd if=/root/big bs=1048576 skip=${MB - 64} 2>/dev/null | md5sum | cut -d' ' -f1; tail -c ${64 << 20} /root/big | md5sum | cut -d' ' -f1`);
  const [a, b] = tail.out.split('\n');
  check('read past 4 GiB at its offset: the same bytes as its tail', tail.code === 0 && a && a === b, tail.out);
  const rm = await sh(t, sys, 'rm /root/big && sync');
  eq('removed and synced', rm.code, 0);
  await sys.shutdown();
  await fs.erase();
}
