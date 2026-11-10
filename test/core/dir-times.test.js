// A folder's time, as a file manager lists it: never 0 (January 1970) for a
// folder a tar, a lazy list, the page or a program made.
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';

export const meta = { hosts: ['browser', 'node'], images: ['busybox'], timeout: 120 };

const LAZY_TIME = 1_700_000_000;

export default async function (t) {
  const { eq, check, timeout } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new MemoryFilesystem(sys);
  // A tar with its folders' own entries.
  fs.addTar(t.work('bb.tar.zst'));
  // A lazy list as a site writes it (`[path, size, url, mtime, mode]`),
  // whose folders no tar names.
  const size = 3 << 20; // lazy.bin (images.mjs)
  fs.addTar(null, { lazy: { base: t.work('lazy.bin'), files: [['/opt/deep/er/lazy.bin', size, 'lazy.bin', LAZY_TIME, 0o644]] } });
  // The page's single files, in folders nothing else names.
  fs.addFiles({ '/srv/page/note.txt': 'from the page\n' });
  const t0 = Math.trunc(Date.now() / 1000);
  await timeout(sys.boot(), 60000, 'boot');
  const env = { PATH: '/bin:/usr/bin', HOME: '/root' };
  const secs = async (p) => Math.trunc((await fs.stat(p)).mtime / 1000);

  check('a tar\'s folder has its own time', (await secs('/bin')) > 0, String(await secs('/bin')));
  eq('a lazy file\'s folder has its time', await secs('/opt/deep/er'), LAZY_TIME);
  eq('and the folder above it', await secs('/opt/deep'), LAZY_TIME);
  check('the page\'s folder is dated now', (await secs('/srv/page')) >= t0 - 5, String(await secs('/srv/page')));
  // A program's folder, as the program sees it.
  const made = await timeout(sys.run(['mkdir', '-p', '/tmp/made/sub'], { env }), 30000, 'mkdir');
  eq('mkdir', made.code, 0);
  const times = [await secs('/tmp/made'), await secs('/tmp/made/sub')];
  check('a program\'s folders are made now', times.every((s) => s >= t0 - 5), JSON.stringify(times));
  // A name made, moved or removed moves its folder's time: the kernel that
  // holds the root has no clock, so each change is dated by its caller's.
  const after = async (cmd) => {
    const r = await timeout(sys.run(['sh', '-c', `sleep 2 && ${cmd}`], { env }), 30000, cmd);
    eq(cmd, r.code, 0);
  };
  const was = await secs('/tmp/made');
  await after('touch /tmp/made/f');
  const added = await secs('/tmp/made');
  check('a file made moves its folder\'s time', added >= was + 2, `${was} -> ${added}`);
  await after('mv /tmp/made/f /tmp/made/sub/g');
  const moved = [await secs('/tmp/made'), await secs('/tmp/made/sub')];
  check('a rename moves both folders\' times', moved.every((s) => s >= added + 2), `${added} -> ${JSON.stringify(moved)}`);
  await after('rm /tmp/made/sub/g');
  const removed = await secs('/tmp/made/sub');
  check('a file removed moves its folder\'s time', removed >= moved[1] + 2, `${moved[1]} -> ${removed}`);
  await after('true');
  await fs.rm('/tmp/made/sub', { recursive: true });
  const page = await secs('/tmp/made');
  check('the page\'s rm moves its folder\'s time', page >= moved[0] + 2, `${moved[0]} -> ${page}`);
  // And one the page makes after boot.
  await fs.mkdir('/srv/api/dir', { recursive: true });
  check('the page\'s mkdir is dated now', (await secs('/srv/api/dir')) >= t0 - 5, String(await secs('/srv/api/dir')));
  await sys.shutdown();
}
