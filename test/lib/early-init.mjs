// The early-init tests' body (test/core/early-init-*.test.js, one system
// each: a page runs one). A bundle laid while its program runs (kfs.js `#layEarly`, app.js
// `streamBundle`): its index first, then the program starts with every file
// named and its bytes still coming. A file not yet laid is there to a lookup
// and `stat`, and a read of it waits for its bytes. When the bundle fails
// (a fetch error, a decode error, an archive that ends short), what never
// came is EIO to the program, never ENOENT; the system's phase is 'failed'
// with the error, and the program goes on.
import { runProgram } from '../../page/src/run.js';
import { setGlobals } from '../../page/src/page-globals.js';
import { untar } from '../../page/src/tar.js';
import { unzstd } from '../../page/src/zfetch.js';
import { bytesOf } from '../../page/runtime/src/internal.js';

const enc = new TextEncoder();

/** The index's records (hosts/kernel `k_fs_seed`: 6 a directory, 5 a file
 *  still to come, 7 a hard link, 2 a symbolic link). */
function indexOf({ dirs, files, hard, links }) {
  const out = [];
  const u32 = (v) => { const b = new Uint8Array(4); new DataView(b.buffer).setUint32(0, v, true); out.push(b); };
  const u64 = (v) => { const b = new Uint8Array(8); new DataView(b.buffer).setBigUint64(0, BigInt(v), true); out.push(b); };
  const text = (s) => { const b = enc.encode(s); u32(b.length); out.push(b); };
  for (const p of dirs) { out.push(Uint8Array.of(6)); u32(0o755); u64(0); text(p); }
  for (const [p, { size, mode }] of Object.entries(files)) { out.push(Uint8Array.of(5)); u32(mode); u64(size); u64(0); text(p); }
  for (const [p, target] of Object.entries(hard)) { out.push(Uint8Array.of(7)); text(target); text(p); }
  for (const [p, target] of Object.entries(links)) { out.push(Uint8Array.of(2)); text(target); text(p); }
  const b = new Uint8Array(out.reduce((n, x) => n + x.length, 0));
  let at = 0;
  for (const x of out) { b.set(x, at); at += x.length; }
  return b;
}

/** `how` the bundle fails: 'fetch', 'decode' or 'short'. */
export async function earlyInit(t, how) {
  const { say, check, eq, until, timeout } = t;
  const dbg = t.debug();
  const sw = Object.fromEntries(Object.entries(dbg).filter(([k]) => k !== 'arch'));
  globalThis.GEX_PAGE_SEARCH = `?${new URLSearchParams(sw)}`;
  globalThis.GEX_ASSET_BASE = t.baseUrl;
  setGlobals((n) => sw[n] ?? null, { arch: dbg.arch ?? 'aarch64' });
  const zbb = await bytesOf(t.work('bb.tar.zst'));
  const tree = untar(await unzstd(zbb));
  // A decoder's own error: a frame cut and its bytes spoilt.
  const bad = zbb.slice(0, Math.min(zbb.length, 4096));
  bad.fill(0x5a, 64);
  const decodeError = await unzstd(bad).then(() => new Error('the spoilt frame decoded'), (e) => e);

  const late = enc.encode('late\n');
  const dirs = [...new Set([...(tree.dirs ?? []), '/etc'])];

  {
    let out = '';
    let keys = null;
    const term = {
      rows: 24, cols: 80,
      write: (s) => { out += typeof s === 'string' ? s : new TextDecoder().decode(s); },
      writeln: (s) => { out += `${s}\n`; },
      onData: (fn) => { keys = fn; return { dispose() { keys = null; } }; },
      onResize: () => ({ dispose() {} }),
      focus() {},
    };
    // The bundle: busybox and /etc/early in its first part, /etc/late in a
    // part that never comes.
    const files = {};
    for (const [p, d] of Object.entries(tree.files)) files[p] = { size: d.length, mode: tree.modes?.[p] ?? 0o644 };
    files['/etc/early'] = { size: 6, mode: 0o644 };
    files['/etc/late'] = { size: late.length, mode: 0o644 };
    const index = indexOf({ dirs, files, hard: tree.hard ?? {}, links: tree.links ?? {} });
    let release;
    const gate = new Promise((r) => { release = r; });
    async function* parts() {
      yield { dirs: [], files: { ...tree.files, '/etc/early': 'early\n' }, modes: {}, links: {}, hard: {}, mtimes: {}, xattrs: {} };
      await gate;
      if (how === 'fetch') throw new TypeError('Failed to fetch');
      if (how === 'decode') throw decodeError;
      // 'short': the archive ends without /etc/late
    }
    let system = null;
    const script = [
      'ls -ln /etc/late | sed "s/^/size /"',
      'ls /etc | grep -c "^late$" | sed "s/^/listed /"',
      'cat /etc/early',
      'echo waiting',
      'cat /etc/late; echo "cat $?"',
      'ls -ln /etc/late | sed "s/^/after /"',
      'cat /etc/late 2>&1 | grep -c "No such file" | sed "s/^/enoent /"',
      'read x; echo "got $x"',
      'exit 3',
    ].join('\n');
    const t0 = performance.now();
    const run = runProgram(term, {
      filesystem: 'memory',
      args: ['/bin/sh', '-c', script],
      stream: { parts: parts(), has: (p) => !!files[p], index: Promise.resolve(index) },
      env: ['PATH=/bin:/usr/bin', 'HOME=/root'],
      cwd: '/',
      onSystem: (s) => { system = s.system; },
    });
    run.catch((e) => { out += `\n[runProgram threw: ${e?.stack ?? e}]`; });
    await timeout(until(() => /^waiting$/m.test(out.replace(/\r/g, '')), 'the program', 60000), 70000, `${how}: start`)
      .catch((e) => { throw new Error(`${e.message}: ${out.slice(-600)} (phase ${system?.state.phase}: ${system?.state.error ?? ''} ${system?.state.console?.slice(-400) ?? ''})`); });
    say(`${how}: the program runs in ${(performance.now() - t0).toFixed(0)} ms, the bundle's last part not in`);
    // The cat of /etc/late waits for its bytes, until the bundle fails.
    await new Promise((r) => setTimeout(r, 500));
    check(`${how}: a read of a file still to come waits`, !/^cat \d+$/m.test(out.replace(/\r/g, '')), out.slice(-300));
    release();
    await timeout(until(() => /^enoent \d+$/m.test(out.replace(/\r/g, '')), 'the rest of the script', 30000), 40000, `${how}: the failure`)
      .catch((e) => { throw new Error(`${e.message}: ${out.slice(-600)}`); });
    eq(`${how}: the system's phase once the bundle failed`, system?.state.phase, 'failed');
    const err = system?.state.error ?? '';
    check(`${how}: the system's error says why`, how === 'fetch' ? /Failed to fetch/.test(err)
      : how === 'decode' ? err.includes(String(decodeError?.message ?? decodeError).slice(0, 40))
        : /ended with 1 of the files its index names missing/.test(err), err.slice(0, 300));
    keys?.('done\r');
    const r = await timeout(run, 30000, `${how}: the end`);
    const lines = out.replace(/\r/g, '');
    say(lines.split('\n').map((l) => `  | ${l}`).join('\n'));
    check(`${how}: a file still to come has its size`, /^size \S+\s+\d+\s+\d+\s+\d+\s+5\s/m.test(lines));
    check(`${how}: and is in its directory`, /^listed 1$/m.test(lines));
    check(`${how}: a file laid reads`, /^early$/m.test(lines));
    check(`${how}: a file that never came is EIO`, /I\/O error/.test(lines) && /^cat 1$/m.test(lines));
    check(`${how}: it is still there, its size kept`, /^after \S+\s+\d+\s+\d+\s+\d+\s+5\s/m.test(lines));
    check(`${how}: never ENOENT`, /^enoent 0$/m.test(lines));
    check(`${how}: the program goes on`, /^got done$/m.test(lines));
    eq(`${how}: the program's status`, r.status, 3);
  }
}
