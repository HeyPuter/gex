// An OPFSFilesystem in a fresh browser context, whose storage is in memory
// (a private window's is too): there the origin private file system stops a
// file near 2 GiB, and growing one past 1 GiB by truncate ends the browser.
// The store grows past 1 GiB (it is grown by writing, not truncated), a
// write past what the context holds is ENOSPC to the program with the page
// and the system still up, and there is room again after a delete and a
// sync (the generation that frees its blocks has room of its own).
import { System } from '../../page/runtime/src/index.js';
import { OPFSFilesystem } from '../../page/runtime/src/fs.js';

export const meta = { hosts: ['browser'], arches: ['a64'], images: ['slim'], timeout: 1800 };

const ENV = { PATH: '/usr/sbin:/usr/bin:/sbin:/bin', HOME: '/root' };

async function sh(t, sys, cmd, ms = 900000) {
  const r = await t.timeout(sys.run(['sh', '-c', cmd], { env: ENV }), ms, cmd.slice(0, 40));
  return { code: r.code, out: (r.stdout + r.stderr).trim() };
}

export default async function (t) {
  const { eq, check, timeout } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const name = `gex-test-full-${t.arch}`;
  await OPFSFilesystem.erase(name);
  const fs = new OPFSFilesystem(sys, { name, cache: 64 });
  fs.addTar(t.work('slim/debian-slim.tar.zst'));
  await timeout(sys.boot(), 60000, 'boot');
  const mid = await sh(t, sys, 'dd if=/dev/zero of=/root/a bs=1048576 count=1536 2>/dev/null && wc -c < /root/a');
  eq('1.5 GiB: the store grown past 1 GiB', [mid.code, mid.out], [0, String(1536 << 20)]);
  const full = await sh(t, sys, 'dd if=/dev/zero of=/root/b bs=1048576 count=1536');
  check('past what the context holds: no space', full.code !== 0 && /No space left/.test(full.out), `${full.code}: ${full.out.slice(-200)}`);
  const rm = await sh(t, sys, 'rm -f /root/a /root/b && sync');
  eq('deleted and synced on a full store', [rm.code, rm.out], [0, '']);
  const again = await sh(t, sys, 'dd if=/dev/zero of=/root/c bs=1048576 count=256 2>/dev/null && wc -c < /root/c');
  check('room again after a delete', again.code === 0 && again.out.endsWith(String(256 << 20)), `${again.code}: ${again.out}`);
  await sys.shutdown();
  await fs.erase();
}
