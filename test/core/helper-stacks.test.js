// The page's own helpers on the global kernel, all at once: persist
// snapshots back to back (8 files of 2 MiB, so each is many kernel calls),
// two fs-opened asks at a time over and over, and keystrokes into the page's
// terminal in a loop, for 30 seconds. Each is a kernel instance on the one
// shared memory with a stack slot and a thread-local block of its own
// (lazy-fetch.mjs HELPER_STACKS, kernelThread); they shared one slot, and
// every instance but the first shared one thread-local block at address 0.
// The terminal runs in a Worker of its own (tty.mjs RemoteTty): on the page's
// thread a key that met a kernel lock a snapshot held threw (Atomics.wait).
// The test's own kernel calls run in a Worker too, on the slot a first guest
// Worker would have.
import { PageTty } from '../../shims/browser/js/tty.mjs';
import { persist } from '../../web/src/persist.js';
import { openedFiles } from '../../web/src/fs-opened.js';
import { HELPER_STACKS, helperStackTop } from '../../shims/browser/js/lazy-fetch.mjs';

export const meta = { hosts: ['browser'], arches: ['x86'], timeout: 240 };

const SECONDS = 30;
const KERNEL = '/gex/gex_kernel.x86.wasm';

// The test's own kernel calls (writing the files, reading the terminal's
// input), in a Worker on a slot of its own (the one the host would give a
// first guest Worker): on the page's thread a lock another holder had would
// throw, which is the bug the terminal's Worker fixes.
const driverSource = (lazy = new URL('../../shims/browser/js/lazy-fetch.mjs', import.meta.url).href, pagefs = new URL('../../shims/browser/js/page-fs.mjs', import.meta.url).href) => `import { kernelThread, lazyFetch, processMemories } from '${lazy}';
import { pageFs } from '${pagefs}';
let k, P;
const put = (s, at) => { const b = new TextEncoder().encode(s); P.set(b, at); return b.length; };
self.onmessage = async (e) => {
  const m = e.data;
  try {
    if (m.op === 'open') {
      let pm;
      for (const shared of [true, false]) {
        pm = new WebAssembly.Memory({ initial: 256, maximum: 65535, shared });
        try {
          k = (await WebAssembly.instantiate(m.module, { env: { memory: m.memory, ...processMemories(pm), lazy_fetch: lazyFetch(m.memory), page_fs: pageFs(m.memory) } })).exports;
          break;
        } catch (err) { if (!shared) throw err; }
      }
      P = new Uint8Array(pm.buffer);
      const area = k.k_stack_area() >>> 0, size = k.k_stack_size() >>> 0, count = k.k_stack_count() >>> 0;
      kernelThread(k, area + size);
      k.k_fs_mkdir(0x100n, put(m.root, 0x100), 0o755);
      k.k_fs_populate_done?.();
      self.postMessage({ area, size, count, tlsSize: k.__tls_size ? k.__tls_size.value >>> 0 : 0,
        tlsAlign: k.__tls_align ? k.__tls_align.value >>> 0 : 16 });
    } else if (m.op === 'write') {
      const bad = [];
      for (const [i, p] of m.names.entries()) {
        const h = k.k_fs_open(0x100n, put(p, 0x100), 0o1101n, 0o644); // O_WRONLY|O_CREAT|O_TRUNC
        if (h < 0) { bad.push('open ' + p + ': ' + h); continue; }
        // Big enough that a snapshot reading them all is many kernel calls.
        const head = new TextEncoder().encode('round ' + m.round + ' file ' + i + '\\n');
        P.fill(0x61 + i, 0x10000, 0x10000 + m.bytes);
        P.set(head, 0x10000);
        if (k.k_fs_write(h, 0n, 0x10000n, m.bytes) !== m.bytes) bad.push('write ' + p);
        k.k_fs_close(h);
        // Read once, so it is in the opened set (TmpFs::opened is what was
        // opened to read) before the asks start: every ask sees the same set.
        const r = k.k_fs_open(0x100n, put(p, 0x100), 0n, 0);
        if (r < 0) bad.push('open ' + p + ' to read: ' + r); else k.k_fs_close(r);
      }
      self.postMessage({ bad });
    } else if (m.op === 'stdin') {
      let text = '';
      for (;;) {
        const n = k.k_stdin_read(0x2000n, 4096);
        if (n <= 0) break;
        text += new TextDecoder().decode(P.slice(0x2000, 0x2000 + n));
      }
      self.postMessage({ text });
    }
  } catch (err) {
    self.postMessage({ error: String(err?.stack ?? err) });
  }
};
`;

export default async function (t) {
  if (!(await fetch(KERNEL, { method: 'HEAD' }).then((r) => r.ok, () => false))) return t.skip(`${KERNEL} is not built`);
  const got = await run().catch((e) => ({ thrown: String(e?.stack ?? e) }));
  if (!t.check('the page ran', !got.thrown, got.thrown?.slice(0, 300))) return;
  t.say(`${got.typed} lines typed (${got.duringSnapshot} while a snapshot ran), ${got.asks} fs-opened pairs, ${got.syncs} snapshots in ${SECONDS} s`);
  t.check('the terminal runs in a Worker of its own (tty.mjs RemoteTty)', got.remote);
  t.check("every helper's results as expected", got.failCount === 0, `${got.failCount} wrong, first ${JSON.stringify(got.fails)}`);
  t.check('every helper ran throughout (over 100 lines, 5 pairs, 5 snapshots)', got.typed > 100 && got.asks > 5 && got.syncs > 5, JSON.stringify({ typed: got.typed, asks: got.asks, syncs: got.syncs }));
  t.check('terminal input arrived while a snapshot ran (50 lines)', got.duringSnapshot >= 50, `${got.duringSnapshot} lines`);
  t.check('every snapshot stored all the files', got.syncErrors.length === 0 && got.shortSyncs === 0 && got.lastFiles >= 8,
    `errors ${JSON.stringify(got.syncErrors)}, short ${got.shortSyncs}, last ${got.lastFiles}`);
  for (const [kind, bad] of Object.entries(got.canaries)) t.check(`${kind}'s stack canary intact`, bad === 0, `${bad} of 64 bytes changed`);
  for (const [kind, x] of Object.entries(got.tls)) t.check(`${kind}'s thread-local block in its slot, panic count 0`, x.inSlot && x.panicCount === 0, JSON.stringify(x));
  t.check("the helpers' thread-local blocks are apart", got.tlsApart);
  t.check('kernel address 0 (the old shared thread-local block) untouched', got.lowUnchanged);
}

async function run() {
  const say = [];
  const fail = (m) => { say.push(m); };

  const module = await WebAssembly.compileStreaming(fetch(KERNEL));
  const memory = new WebAssembly.Memory({ initial: 2048, maximum: 65536, shared: true });
  const K = () => new Uint8Array(memory.buffer);
  const driver = new Worker(URL.createObjectURL(new Blob([driverSource()], { type: 'text/javascript' })), { type: 'module' });
  const ask = (m) => new Promise((resolve, reject) => {
    driver.onmessage = (e) => (e.data.error ? reject(new Error(e.data.error)) : resolve(e.data));
    driver.postMessage(m);
  });
  const ROOTDIR = '/keep';
  const N = 8, BYTES = 2 << 20;
  const names = Array.from({ length: N }, (_, i) => ROOTDIR + '/f' + i + '.txt');
  const geo = await ask({ op: 'open', module, memory, root: ROOTDIR });
  const write = async (round) => { for (const b of (await ask({ op: 'write', names, round, bytes: BYTES })).bad) fail(b); };
  await write(0);

  // The slots, their canaries at the deep end, and where each block goes.
  const k = { k_stack_area: () => geo.area, k_stack_size: () => geo.size, k_stack_count: () => geo.count };
  const { size, tlsSize } = geo;
  const tlsAlign = Math.max(16, geo.tlsAlign);
  const kinds = ['tty', 'persist', 'fsOpened'];
  const slot = {};
  for (const kind of kinds) {
    const top = helperStackTop(k, kind);
    const canary = top - size;
    for (let i = 0; i < 64; i++) K()[canary + i] = (0xa5 ^ (i * 31) ^ HELPER_STACKS[kind]) & 255;
    slot[kind] = { top, canary, tls: tlsSize ? (top - tlsSize) & -tlsAlign : 0 };
  }
  const low0 = Array.from(K().subarray(0, 32)).join();

  const spec = { module, memory };
  const tty = await PageTty.open(spec);
  if (!tty) throw new Error('no PageTty');
  const remote = tty.ready() instanceof Promise;
  const syncs = [];
  const p = persist('helper-stacks-' + Date.now(), ROOTDIR, spec, { every: 1000, onSync: (m) => syncs.push(m) });
  const end = performance.now() + SECONDS * 1000;
  let typed = 0, asks = 0, round = 0, readBack = '', duringSnapshot = 0, snapshotting = false;
  const want = [];
  const asking = (async () => {
    while (performance.now() < end) {
      const [a, b] = await Promise.all([openedFiles(spec), openedFiles(spec)]);
      asks++;
      if (a.join() !== b.join()) fail('fs-opened pair ' + asks + ' differs');
      for (const f of names) if (!a.includes(f)) { fail('fs-opened ' + asks + ' has no ' + f); break; }
    }
  })();
  // A snapshot in flight all the time, so keys meet one holding the kernel's
  // locks: one after another, each reading 16 MiB through the kernel.
  const snapping = (async () => {
    while (performance.now() < end) {
      snapshotting = true;
      const m = await p.sync();
      snapshotting = false;
      if (m === null) await new Promise((r) => setTimeout(r, 1));
    }
  })();
  while (performance.now() < end) {
    const line = 'line ' + typed + ' abcdefghij';
    let echo = '';
    const at = snapshotting;
    for (const ch of [...line, '\r']) {
      const r = await tty.key(ch);
      if (r.echo) echo += new TextDecoder().decode(r.echo);
    }
    if (at && snapshotting) duringSnapshot++;
    if (echo !== line + '\r\n') fail('echo of ' + JSON.stringify(line) + ' was ' + JSON.stringify(echo));
    want.push(line + '\n');
    readBack += (await ask({ op: 'stdin' })).text;
    typed++;
    if (typed % 200 === 0) await write(++round);
  }
  await asking;
  await snapping;
  const last = await p.sync();
  if (readBack !== want.join('')) fail('terminal input read back ' + readBack.length + ' bytes, typed ' + want.join('').length);
  const errors = syncs.filter((m) => m.error);
  const short = syncs.filter((m) => !m.error && !m.skipped && m.files < N);
  const checks = {
    typed, asks, syncs: syncs.length, round, duringSnapshot, remote,
    syncErrors: errors.map((m) => m.error).slice(0, 3),
    shortSyncs: short.length,
    lastFiles: last?.files ?? null,
    canaries: Object.fromEntries(kinds.map((kind) => {
      const c = slot[kind].canary;
      let bad = 0;
      for (let i = 0; i < 64; i++) bad += K()[c + i] !== ((0xa5 ^ (i * 31) ^ HELPER_STACKS[kind]) & 255);
      return [kind, bad];
    })),
    tls: Object.fromEntries(kinds.map((kind) => {
      const { top, tls } = slot[kind];
      const words = new Uint32Array(memory.buffer, tls, 2);
      return [kind, { inSlot: tls >= top - size && tls + tlsSize <= top, panicCount: words[0] + words[1] }];
    })),
    tlsApart: new Set(kinds.map((kind) => slot[kind].tls)).size === kinds.length && tlsSize > 0,
    lowUnchanged: Array.from(K().subarray(0, 32)).join() === low0,
    fails: say.slice(0, 8), failCount: say.length,
  };
  p.stop();
  tty.close?.();
  return checks;
}
