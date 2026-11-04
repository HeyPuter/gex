// The root kept across loads in an OPFSFilesystem's store
// (gex_kernel::vfs::tmpfs::persist). Each phase is a load of its own:
//   1. a fresh store: files written, a mode changed, a source's file and
//      directory removed, a directory, a symlink and a hard link made;
//      synced, then the page reloads without a shutdown (a killed tab);
//   2. all of it is there; a second generation is written, then its last
//      journal segment's checksum broken in the store;
//   3. the boot says it fell back, and lands on the first generation; a
//      generation written, then the newest superblock's checksum broken;
//   4. the boot falls back again, to the one before.
import { System } from '../../page/runtime/src/index.js';
import { OPFSFilesystem } from '../../page/runtime/src/fs.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 400 };

const ENV = { PATH: '/bin:/usr/bin', HOME: '/root' };
const BLOCK = 1 << 16;
const BIG = 3 << 20;

/** The same bytes at every load: no two 64 KiB blocks alike. */
function pattern(n) {
  const b = new Uint8Array(n);
  for (let i = 0; i < n; i++) b[i] = (i * 31 + (i >>> 16) * 7) & 255;
  return b;
}

function same(a, b) {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

async function sh(t, sys, cmd) {
  const r = await t.timeout(sys.run(['sh', '-c', cmd], { env: ENV }), 120000, cmd.slice(0, 40));
  return { code: r.code, out: (r.stdout + r.stderr).trim() };
}

/** The newest superblock in the store: its place, generation, head block. */
async function newest(fs) {
  const b = await fs.read(0, BLOCK);
  const v = new DataView(b.buffer, b.byteOffset, b.length);
  let best = null;
  for (const at of [0, BLOCK / 2]) {
    if (new TextDecoder().decode(b.subarray(at, at + 7)) !== 'gexroot') continue;
    const gen = Number(v.getBigUint64(at + 16, true));
    if (!best || gen > best.gen) best = { at, gen, head: v.getUint32(at + 24, true) };
  }
  return best;
}

/** One byte of the store changed, once the stopped system's store Worker
 *  has let the file go. */
async function flip(fs, at) {
  const [x] = await fs.read(at, 1);
  for (let i = 0; ; i++) {
    try {
      return await fs.write(at, new Uint8Array([x ^ 0xff]));
    } catch (e) {
      if (i > 50) throw e;
      await new Promise((r) => setTimeout(r, 100));
    }
  }
}

export default async function (t) {
  const { eq, check, timeout } = t;
  // One store per arch: the two may run at once, in one origin.
  const STORE = `gex-test-persist-${t.arch}`;
  if (t.phase === 1) await OPFSFilesystem.erase(STORE);
  const said = [];
  const error = console.error;
  console.error = (...a) => { said.push(a.join(' ')); error.apply(console, a); };
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new OPFSFilesystem(sys, { name: STORE, cache: 16, writeback: 600000 });
  fs.addTar(t.work('bb.tar.zst'));
  await timeout(sys.boot(), 60000, 'boot');
  console.error = error;
  const fellBack = said.some((l) => /last generation was torn or failed its checks; booted the one before it/.test(l));

  // What the first generation holds.
  const first = async (what) => {
    const r = await sh(t, sys, 'cat /root/small; ls -l /root/link | sed "s/.*-> //"; set -- $(ls -l /etc/xattr); echo $1; test -e /bin/true && echo true-kept; test -d /root/dir && echo dir; set -- $(ls -l /root/big); echo $2; test -e /root/second && echo second');
    eq(`${what}: a program's file, a symlink, a mode, a removal, a directory, a hard link`, r.out.split('\n'), ['hello', 'small', '-rw-------', 'dir', '2']);
    check(`${what}: 3 MiB written through a 16 MiB cache, read back`, same(await fs.readFile('/root/big'), pattern(BIG)));
    check(`${what}: and its hard link`, same(await fs.readFile('/root/also'), pattern(BIG)));
  };

  if (t.phase === 1) {
    check('a fresh store: nothing said', !said.length, said.join('; '));
    const r = await sh(t, sys, 'mkdir -p /root/dir && echo hello > /root/small && ln -s small /root/link && chmod 600 /etc/xattr && rm /bin/true');
    eq('the changes', r.code, 0);
    await fs.writeFile('/root/big', pattern(BIG));
    eq('a hard link', (await sh(t, sys, 'ln /root/big /root/also')).code, 0);
    await sys.sync();
    // Killed: no shutdown, no last sync.
    return t.reload();
  }
  if (t.phase === 2) {
    check('a whole store: nothing said', !fellBack, said.join('; '));
    await first('after a reload without a shutdown');
    const before = await sh(t, sys, 'echo second > /root/second');
    eq('a second generation', before.code, 0);
    await sys.sync();
    await sys.shutdown();
    const sb = await newest(fs);
    check('the store has a superblock', !!sb && sb.head > 0, JSON.stringify(sb));
    // A byte of the last segment's records: its checksum fails.
    await flip(fs, (sb.head - 1) * BLOCK + 40);
    return t.reload();
  }
  if (t.phase === 3) {
    check('a torn journal segment: the boot says it fell back', fellBack, said.join('; ') || 'nothing said');
    await first('the generation before the torn one');
    eq('the torn generation\'s file is not there', (await sh(t, sys, 'test -e /root/second && echo there || echo gone')).out, 'gone');
    eq('a generation after it', (await sh(t, sys, 'echo third > /root/third')).code, 0);
    await sys.sync();
    eq('which a reload would keep', (await sh(t, sys, 'cat /root/third')).out, 'third');
    await sys.shutdown();
    const sb = await newest(fs);
    // The newest superblock's generation number: its checksum fails.
    await flip(fs, sb.at + 16);
    return t.reload();
  }
  check('a torn superblock: the boot says it fell back', fellBack, said.join('; ') || 'nothing said');
  await first('the generation before the torn superblock');
  eq('the newest generation\'s file is not there', (await sh(t, sys, 'test -e /root/third && echo there || echo gone')).out, 'gone');
  await sys.shutdown();
  await fs.erase();
}
