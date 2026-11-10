// Extended attributes kept across loads in an OPFSFilesystem's store
// (gex_kernel::vfs::tmpfs::persist): what gex records on a program
// (user.gex.mem64, the 64-bit memory its next start gets) must survive a
// reload, or every visit repeats the crash that made it. Two loads:
//   1. a fresh store: the attribute set on a file the tar laid (unchanged
//      bytes), on a file of a tar laid after boot, and on a file written
//      here; synced, then the page reloads without a shutdown;
//   2. each file has it, and the tar's file still has the tar's bytes.
import { System } from '../../page/runtime/src/index.js';
import { OPFSFilesystem } from '../../page/runtime/src/fs.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 300 };

const ENV = { PATH: '/bin:/usr/bin', HOME: '/root' };
const FILES = ['/etc/xattr', '/opt/x/etc/xattr', '/root/mine'];

export default async function (t) {
  const { eq, timeout } = t;
  const STORE = `gex-test-persist-xattr-${t.arch}`;
  if (t.phase === 1) await OPFSFilesystem.erase(STORE);
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new OPFSFilesystem(sys, { name: STORE, writeback: 600000 });
  fs.addTar(t.work('bb.tar.zst'));
  await timeout(sys.boot(), 60000, 'boot');
  await fs.addTar(t.work('bb.tar.zst'), { at: '/opt/x' });
  const run = async (args) => {
    const r = await timeout(sys.run(args, { env: ENV }), 60000, args.join(' ').slice(0, 40));
    return (r.stdout + r.stderr).trim();
  };
  const tarBytes = 'cat /etc/xattr | md5sum | cut -c1-32';
  if (t.phase === 1) {
    eq('a file written here', await run(['sh', '-c', 'echo mine > /root/mine']), '');
    for (const f of FILES) eq(`set on ${f}`, await run(['rt-probe', 'setxattr', f, 'user.gex.mem64', '1']), 'set');
    await fs.writeFile('/root/tar-sum', new TextEncoder().encode(await run(['sh', '-c', tarBytes])));
    await sys.sync();
    // Killed: no shutdown, no last sync.
    return t.reload();
  }
  for (const f of FILES) eq(`after a reload: ${f} keeps it`, await run(['rt-probe', 'xattr', f, 'user.gex.mem64']), '1');
  eq('the tar\'s other attribute too', await run(['rt-probe', 'xattr', '/etc/xattr', 'user.rt']), 'kept');
  eq('and the tar\'s bytes', await run(['sh', '-c', tarBytes]), new TextDecoder().decode(await fs.readFile('/root/tar-sum')));
  eq('the file written here keeps its bytes', await run(['cat', '/root/mine']), 'mine');
  await sys.shutdown();
  await fs.erase();
}
