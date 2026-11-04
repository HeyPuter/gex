// A native program's mount(2) and umount2(2) by their own ABI's numbers
// (x86-64 165 and 166, aarch64 40 and 39): a tmpfs over a directory, a file
// in it, the mount listed, gone at the unmount, a second unmount EINVAL
// (test/fixtures/programs/mount-tmpfs.c).
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';

export const meta = { hosts: ['browser', 'node'], images: ['busybox', 'native-progs'], timeout: 120 };

const WANT = `mount: 0 ok
write: ok
listed: 1
umount: 0 ok
gone: yes
listed: 0
umount again: -1 Invalid argument`;

export default async function (t) {
  const { eq, timeout } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  fs.addFiles({ '/g/mount-tmpfs': new URL(t.work(`mount-tmpfs.${t.arch === 'x86' ? 'x86' : 'a64'}`)) }, { mode: 0o755 });
  await timeout(sys.boot(), 60000, 'boot');
  const r = await timeout(sys.run(['/g/mount-tmpfs'], { env: { PATH: '/bin', HOME: '/' } }), 60000, 'mount-tmpfs');
  eq('mount, a file in it, umount', r.stdout.trim(), WANT, r.stderr.slice(-2000));
  await sys.shutdown();
}
