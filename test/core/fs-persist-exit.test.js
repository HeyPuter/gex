// A program's files are kept once it has exited (gex_kernel::vfs::tmpfs::
// persist::exited): an installer-like process writes a program's files and
// ends, and the page reloads at once, as a killed tab: without a shutdown, a
// sync (the page's flush on pagehide does nothing here) or a write-back tick
// (ten minutes here). The next load has every file, byte for byte.
import { System } from '../../page/runtime/src/index.js';
import { OPFSFilesystem } from '../../page/runtime/src/fs.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 300 };

const ENV = { PATH: '/bin:/usr/bin', HOME: '/root' };
// what the installer writes: a large file (blocks in the store) and small ones
const INSTALL = 'mkdir -p /root/app/lib && seq 1 400000 > /root/app/app.bin && echo dll > /root/app/lib/a.dll && echo cfg > /root/app/app.cfg';
// the large file's size, its lines and their sum
const SUM = 'wc -c < /root/app/app.bin; awk \'{ s += $1 } END { printf "%d %.0f\\n", NR, s }\' /root/app/app.bin';

export default async function (t) {
  const { eq, timeout } = t;
  // One store per arch: the two may run at once, in one origin.
  const STORE = `gex-test-persist-exit-${t.arch}`;
  if (t.phase === 1) await OPFSFilesystem.erase(STORE);
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new OPFSFilesystem(sys, { name: STORE, cache: 16, writeback: 600000 });
  fs.addTar(t.work('bb.tar.zst'));
  await timeout(sys.boot(), 60000, 'boot');
  const sh = async (cmd) => {
    const r = await timeout(sys.run(['sh', '-c', cmd], { env: ENV }), 120000, cmd.slice(0, 40));
    return { code: r.code, out: (r.stdout + r.stderr).trim() };
  };

  if (t.phase === 1) {
    // A killed tab: the page's own flush when it goes (pagehide's sync)
    // writes nothing, so only what the exit kept is there.
    sys.sync = async () => {};
    const r = await sh(INSTALL);
    eq('the installer ran', [r.code, r.out], [0, '']);
    // reloaded the moment it has ended: no shutdown, no sync, no tick
    return t.reload();
  }
  eq('after a reload at once after the installer exited: its large file', (await sh(SUM)).out, '2688895\n400000 80000200000');
  eq('and its small ones', (await sh('cat /root/app/lib/a.dll /root/app/app.cfg')).out, 'dll\ncfg');
  await OPFSFilesystem.erase(STORE).catch(() => {});
}
