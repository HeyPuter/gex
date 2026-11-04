// The filesystem's write-back (fs.js, gex_kernel::vfs::blocks): what a
// program writes stays in the cache until fsync, sync(2), the page's
// sys.sync() or the write-back's age sends it to the store with a generation
// of the kept root (gex_kernel::vfs::tmpfs::persist), and each of those
// flushes the store; a write of a MiB or more goes to the store at once; a
// full store is ENOSPC, a failing one EIO. Seen from a
// store of the page's own, which counts what reaches it. One system per page,
// so each phase is a load of its own (t.reload).
import { System } from '../../page/runtime/src/index.js';
import { Filesystem } from '../../page/runtime/src/fs.js';

export const meta = { hosts: ['browser'], images: ['slim'], timeout: 300,
  single: "a store of the page's own needs the default build" };

const ENV = { PATH: '/usr/bin:/bin', HOME: '/root' };
const MB = 1 << 20;

/** A store in pieces in the page's memory that counts what reaches it:
 *  `fail` makes writes throw, `room` bounds its length (ENOSPC past it). */
class CountingStore extends Filesystem {
  #pieces = new Map();
  #size = 0;
  written = 0;
  flushes = 0;
  fail = false;
  room = Infinity;
  #each(offset, length, fn) {
    for (let at = 0; at < length;) {
      const p = offset + at, n = Math.min(length - at, MB - (p % MB));
      fn(Math.floor(p / MB), p % MB, at, n);
      at += n;
    }
  }
  async read(offset, length) {
    const out = new Uint8Array(Math.max(0, Math.min(length, this.#size - offset)));
    this.#each(offset, out.length, (i, o, at, n) => { const p = this.#pieces.get(i); if (p) out.set(p.subarray(o, o + n), at); });
    return out;
  }
  async write(offset, bytes) {
    if (this.fail) throw new Error('the store is failing');
    this.#each(offset, bytes.length, (i, o, at, n) => {
      if (!this.#pieces.has(i)) this.#pieces.set(i, new Uint8Array(MB));
      this.#pieces.get(i).set(bytes.subarray(at, at + n), o);
    });
    this.#size = Math.max(this.#size, offset + bytes.length);
    this.written += bytes.length;
  }
  async size() { return this.#size; }
  async truncate(length) {
    if (length > this.room) throw Object.assign(new Error('full'), { code: 'ENOSPC' });
    for (const i of this.#pieces.keys()) if (i * MB >= length) this.#pieces.delete(i);
    this.#size = length;
  }
  async flush() { this.flushes++; }
}

async function sh(t, sys, cmd) {
  const r = await t.timeout(sys.run(['bash', '-c', cmd], { env: ENV }), 120000, cmd.slice(0, 40));
  return { code: r.code, out: r.stdout + r.stderr };
}

export default async function (t) {
  const { eq, check, timeout } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const opts = [
    { cache: 64, writeback: 600000 },
    { cache: 64, writeback: 1000 },
    { cache: 16, writeback: 600000 },
  ][t.phase - 1];
  const fs = new CountingStore(sys, opts);
  fs.addTar(t.work('slim/debian-slim.tar.zst'));
  await timeout(sys.boot(), 120000, 'boot');
  eq('boot writes nothing to the store', fs.written, 0);
  if (t.phase === 1) {
    // fsync (dd conv=fsync), then fdatasync.
    let r = await sh(t, sys, 'dd if=/dev/zero of=/root/a bs=65536 count=32 2>/dev/null');
    eq('a write', r.code, 0);
    eq('stays in the cache', fs.written, 0);
    r = await sh(t, sys, 'dd if=/dev/zero of=/root/b bs=65536 count=32 conv=fsync 2>/dev/null');
    eq('dd conv=fsync', r.code, 0);
    // A generation of the kept root: what else was dirty goes with it.
    check('fsync: the file reached the store and the store was flushed', fs.written >= 4 * MB && fs.flushes >= 2, `${fs.written} bytes, ${fs.flushes} flushes`);
    const w = fs.written, f = fs.flushes;
    r = await sh(t, sys, 'dd if=/dev/zero of=/root/c bs=65536 count=16 conv=fdatasync 2>/dev/null');
    check('fdatasync too', r.code === 0 && fs.written >= w + MB && fs.flushes > f, `${r.code}: ${fs.written - w} bytes, ${fs.flushes - f} flushes`);
    // sync(2) and syncfs(2) (sync -f): what is still only in the cache.
    for (const [what, cmd] of [['sync', 'sync'], ['syncfs', 'sync -f /root']]) {
      const n = what === 'sync' ? 'f' : 'g';
      r = await sh(t, sys, `dd if=/dev/zero of=/root/${n} bs=65536 count=16 2>/dev/null`);
      const w1 = fs.written, f1 = fs.flushes;
      r = await sh(t, sys, cmd);
      check(`${what}: the rest reached the store, flushed`, r.code === 0 && fs.written >= w1 + MB && fs.flushes > f1, `${r.code} ${r.out}: ${fs.written - w1} bytes, ${fs.flushes - f1} flushes`);
    }
    // A write of a MiB or more goes straight to the store, not the cache.
    const w3 = fs.written;
    r = await sh(t, sys, 'dd if=/dev/zero of=/root/j bs=4194304 count=1 2>/dev/null');
    check('a 4 MiB write: in the store when it returns', r.code === 0 && fs.written >= w3 + 4 * MB, `${r.code}: ${fs.written - w3} bytes`);
    // The page's sync.
    r = await sh(t, sys, 'dd if=/dev/zero of=/root/h bs=65536 count=16 2>/dev/null');
    const w2 = fs.written, f2 = fs.flushes;
    await sys.sync();
    check('sys.sync(): the rest reached the store, flushed', fs.written >= w2 + MB && fs.flushes > f2, `${fs.written - w2} bytes, ${fs.flushes - f2} flushes`);
    // A failing store: the write-out is EIO to fsync, and the data stays.
    fs.fail = true;
    r = await sh(t, sys, 'dd if=/dev/urandom of=/root/d bs=65536 count=4 conv=fsync');
    check('a failing store: fsync is EIO', r.code !== 0 && /I\/O error/i.test(r.out), `${r.code}: ${r.out}`);
    fs.fail = false;
    r = await sh(t, sys, 'dd if=/root/d of=/root/e 2>/dev/null && dd if=/root/e of=/dev/null bs=65536 2>&1 | head -1');
    check('and what was written is still there', r.code === 0 && /^4\+0 records in/.test(r.out), `${r.code}: ${r.out}`);
    await sys.sync();
  } else if (t.phase === 2) {
    // The write-back's age: there without a sync, within a few seconds. (A
    // slow dd may already see some of it out: the file's creation ages too,
    // and the generation it makes takes every dirty block with it.)
    const r = await sh(t, sys, 'dd if=/dev/zero of=/root/a bs=65536 count=32 2>/dev/null');
    eq('a write', r.code, 0);
    // (The flush follows the blocks' writes: wait for both.)
    await t.until(() => fs.written >= 2 * MB && fs.flushes >= 1, 'the write-back: the blocks reach the store within 8 s of a 1 s age, and it is flushed', 8000);
  } else {
    // A full store: ENOSPC to the program, and room again after a delete.
    fs.room = 8 * MB;
    const full = await sh(t, sys, 'dd if=/dev/zero of=/root/a bs=1048576 count=96');
    check('a full store: no space', full.code !== 0 && /No space left/.test(full.out), `${full.code}: ${full.out}`);
    fs.room = Infinity;
    const again = await sh(t, sys, 'rm /root/a && dd if=/dev/zero of=/root/a bs=1048576 count=4 2>/dev/null && wc -c < /root/a');
    eq('room again', [again.code, again.out.trim()], [0, String(4 * MB)]);
  }
  await sys.shutdown();
  if (t.phase < 3) return t.reload();
}
