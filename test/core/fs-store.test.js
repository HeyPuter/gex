// Where the files' contents are kept (fs.js): an OPFSFilesystem with a cache
// far smaller than what a program writes, a MemoryFilesystem with a max, and
// the abstract Filesystem a page's own store subclasses. One system per
// page, so each is a load of its own (t.reload).
import { System } from '../../page/runtime/src/index.js';
import { Filesystem, MemoryFilesystem, OPFSFilesystem } from '../../page/runtime/src/fs.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 300 };

const ENV = { PATH: '/bin:/usr/bin', HOME: '/root' };

/** Whether two byte arrays are the same. */
function indexedEqual(a, b) {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

/** `sh -c cmd`'s exit and output. */
async function sh(t, sys, cmd) {
  const r = await t.timeout(sys.run(['sh', '-c', cmd], { env: ENV }), 120000, cmd.slice(0, 40));
  return { code: r.code, out: r.stdout + r.stderr };
}

export default async function (t) {
  const { eq, check, timeout } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  if (t.phase === 1) {
    check('new Filesystem(sys) throws: it is abstract', (() => { try { new Filesystem(sys); return false; } catch (e) { return e instanceof TypeError && /abstract/.test(e.message); } })());
    class Half extends Filesystem { async read() { return new Uint8Array(0); } }
    check('a subclass without its store throws', (() => { try { new Half(sys); return false; } catch (e) { return /write, size, truncate, flush/.test(e.message); } })());
    check('OPFSFilesystem names its store', (() => { try { new OPFSFilesystem(sys); return false; } catch (e) { return /name the store/.test(e.message); } })());
    // 40 MiB through a 16 MiB cache: written out to the store and read back.
    // One store per arch (the two may run at once, in one origin), empty.
    const name = `gex-test-store-${t.arch}`;
    await OPFSFilesystem.erase(name);
    const fs = new OPFSFilesystem(sys, { name, cache: 16 });
    fs.addTar(t.work('bb.tar.zst'));
    await timeout(sys.boot(), 60000, 'boot');
    const r = await sh(t, sys, 'dd if=/dev/urandom of=/root/a bs=1048576 count=40 2>/dev/null && cp /root/a /root/b');
    eq('OPFS: 40 MiB written and copied through a 16 MiB cache', [r.code, r.out], [0, '']);
    const [a, b] = [await fs.readFile('/root/a'), await fs.readFile('/root/b')];
    check('OPFS: the page reads both back, the same', a.length === 40 << 20 && indexedEqual(a, b), `${a.length} and ${b.length} bytes`);
    const synced = await sys.sync().then(() => 'ok', (e) => e.code ?? e.message);
    eq('OPFS: sys.sync() writes the cache out to the store', synced, 'ok');
    await sys.shutdown();
    await fs.erase();
    return t.reload();
  }
  // A max: a write past it is ENOSPC, and room comes back with a delete.
  const fs = new MemoryFilesystem(sys, { max: 48 << 20 });
  fs.addTar(t.work('bb.tar.zst'));
  await timeout(sys.boot(), 60000, 'boot');
  const full = await sh(t, sys, 'dd if=/dev/zero of=/root/a bs=1048576 count=64');
  check('memory max: a write past it fails, no space', full.code !== 0 && /No space left/.test(full.out), `${full.code}: ${full.out}`);
  const again = await sh(t, sys, 'rm /root/a && dd if=/dev/zero of=/root/a bs=1048576 count=16 2>/dev/null && wc -c < /root/a');
  eq('memory max: room again after a delete', [again.code, again.out.trim()], [0, String(16 << 20)]);
  await sys.shutdown();
}
