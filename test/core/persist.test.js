// IdbPersist: the root's changes kept across a reload, and nothing else.
import { System } from '../../web/runtime/src/index.js';
import { Filesystem } from '../../web/runtime/src/fs.js';
import { IdbPersist } from '../../web/runtime/src/persist/idb.js';
import { MemFs } from '../fixtures/memfs.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 120 };

const DB = 'rt-persist-test';

export default async function (t) {
  const { say, check, eq, timeout, hits } = t;
  const phase = t.phase;
  if (phase === 1) {
    await new Promise((r, j) => { const q = indexedDB.deleteDatabase(DB); q.onsuccess = r; q.onerror = j; });
  }
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new Filesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  fs.addFiles({ '/etc/seeded': 'from the sources\n', '/etc/gone/a': 'a\n', '/etc/gone/b/c': 'c\n' });
  // A lazy file nobody writes: a snapshot must not fetch it.
  const size = 3 << 20;
  fs.addTar(null, { lazy: [['/opt/lazy.bin', size, t.work('lazy.bin')]] });
  fs.mount('/mnt/m', new MemFs({ '/x': 'mounted\n' }));
  const p = new IdbPersist(fs, { db: DB, quiet: 300 });
  await timeout(sys.boot(), 60000, 'boot');
  const env = { PATH: '/bin:/usr/bin', HOME: '/root' };
  const sh = async (cmd) => (await timeout(sys.run(['sh', '-c', cmd], { env }), 30000, cmd));
  const out = async (cmd) => (await sh(cmd)).stdout;

  if (phase === 1) {
    const r = await sh([
      'echo kept > /root/new.txt',
      'printf "#!/bin/sh\\necho ran\\n" > /root/run.sh && chmod 755 /root/run.sh',
      'mkdir -p /root/dir/sub && chmod 700 /root/dir',
      'ln -s new.txt /root/link',
      'chmod 600 /etc/seeded',
      'rm -r /etc/gone',
      'rm /etc/xattr',
      'echo scratch > /tmp/scratch && mkdir -p /run/x && echo r > /run/x/f',
      'echo m2 > /mnt/m/y',
    ].join(' && '));
    eq('changes made', r.code, 0);
    const before = await hits('lazy.bin');
    // A save of its own, about a second after the changes settle.
    const ev = await timeout(new Promise((res) => p.addEventListener('stored', (e) => res(e), { once: true })), 30000, 'stored');
    check(`stored after the changes settled (${ev.stored} records)`, ev.stored > 0);
    const s = await p.save();
    eq('a second save has nothing new', [s.stored, s.dropped], [0, 0]);
    eq('the lazy file was not fetched to save it', await hits('lazy.bin'), before);
    await sys.shutdown();
    return t.reload();
  }

  if (phase === 2) {
    eq('a file', await out('cat /root/new.txt'), 'kept\n');
    eq('its mode', await out('/root/run.sh'), 'ran\n');
    eq('a directory and its mode', (await fs.stat('/root/dir')).mode, 0o700);
    eq('an empty directory', (await fs.stat('/root/dir/sub')).type, 'directory');
    eq('a symlink', await fs.readlink('/root/link'), 'new.txt');
    eq('a seeded file\'s mode', (await fs.stat('/etc/seeded')).mode, 0o600);
    eq('its bytes from the sources', await out('cat /etc/seeded'), 'from the sources\n');
    eq('a deleted directory', (await sh('ls /etc/gone')).code !== 0, true);
    eq('a deleted file', (await sh('ls /etc/xattr')).code !== 0, true);
    eq('/tmp and /run are not kept', [(await sh('ls /tmp/scratch')).code !== 0, (await sh('ls /run/x')).code !== 0], [true, true]);
    eq('a mount is not kept (its provider keeps it)', (await sh('ls /mnt/m/y')).code !== 0, true);
    eq('the lazy file is still there', (await fs.stat('/opt/lazy.bin')).size, size);
    // Change what was restored: a rewrite, a delete.
    await sh('echo changed > /root/new.txt && rm /root/link');
    const s = await p.save();
    check(`a save after restoring (${s.stored} stored, ${s.dropped} dropped)`, s.stored === 2 && s.dropped === 0);
    await sys.shutdown();
    return t.reload();
  }

  eq('a restored file rewritten', await out('cat /root/new.txt'), 'changed\n');
  eq('a restored symlink deleted', (await sh('ls /root/link')).code !== 0, true);
  eq('still a directory', (await fs.stat('/root/dir')).mode, 0o700);
  await p.reset();
  await sys.shutdown();
  const left = await new Promise((r) => { const q = indexedDB.open(DB); q.onsuccess = () => { const n = q.result.objectStoreNames.length; q.result.close(); r(n); }; });
  eq('reset() deletes it', left, 0);
  say('done');
}
