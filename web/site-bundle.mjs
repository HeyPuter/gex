// A 32-bit Wine program's bundle as a site ships it, for every site builder (web/eject-site.mjs,
// web/page-site.mjs): the tar extracted, its .wasm stripped, what the browser never
// reads pruned, the bundle split (the files the recorded run opened stay in it, the
// rest become lazy files the page reads from the host when a guest first opens one),
// and the tar written again, deduplicated by content.
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { chmodSync, copyFileSync, existsSync, lstatSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, utimesSync, writeFileSync } from 'node:fs';
import { dirname, join, relative } from 'node:path';

const sha = (b) => createHash('sha256').update(b).digest('hex');

/** Each regular file (or symbolic link) under DIR: fn(path, path relative to BASE). */
export function walk(dir, fn, base = dir) {
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    if (e.isDirectory()) walk(p, fn, base);
    else if (e.isFile() || e.isSymbolicLink()) fn(p, relative(base, p));
  }
}

/** A .wasm's copy without its debug sections (in WORK/strip, by content), or
 *  SRC itself when `on` is false or it is no .wasm. */
export function stripper(work, on) {
  return (src) => {
    if (!on || !src.endsWith('.wasm')) return src;
    const dst = join(work, 'strip', sha(readFileSync(src)));
    mkdirSync(dirname(dst), { recursive: true });
    if (!existsSync(dst)) execFileSync('wasm-tools', ['strip', '-d', '^(\\.debug_.*|producers|target_features|external_debug_info|sourceMappingURL)$', src, '-o', dst]);
    return dst;
  };
}

/**
 * TAR extracted into WORK/tarx. A read-only directory (a prefix's Start menu
 * folders, which the shell reads desktop.ini names from only when read only)
 * is made writable while files leave it, and packed with its own mode again
 * (`pack`). Every directory's times are the bundle's again when it is packed:
 * the files that leave it (the split) do not stamp it with the build's time,
 * which a program may key a cache on (KDE's app database, a Windows app's),
 * and the site is the same bytes each build.
 */
export function extract(tar, work) {
  const x = join(work, 'tarx');
  mkdirSync(x, { recursive: true });
  execFileSync('tar', ['-xf', tar, '-C', x]);
  const readOnly = [];
  const unlock = (d) => {
    for (const e of readdirSync(d, { withFileTypes: true })) {
      if (!e.isDirectory()) continue;
      const p = join(d, e.name);
      const mode = statSync(p).mode & 0o7777;
      if (!(mode & 0o200)) { readOnly.push([p, mode]); chmodSync(p, mode | 0o700); }
      unlock(p);
    }
  };
  unlock(x);
  const dirTimes = [];
  const times = (d) => {
    for (const e of readdirSync(d, { withFileTypes: true })) {
      if (!e.isDirectory()) continue;
      const p = join(d, e.name);
      const st = statSync(p);
      dirTimes.push([p, st.atime, st.mtime]);
      times(p);
    }
  };
  times(x);
  return { x, readOnly, dirTimes };
}

/** --strip: every .wasm of the tree stripped, with the file's own times. */
export function stripTree(x, stripped) {
  walk(x, (p) => {
    if (!p.endsWith('.wasm')) return;
    const st = statSync(p);
    copyFileSync(stripped(p), p);
    utimesSync(p, st.atime, st.mtime);
  });
}

/** --prune: the files the browser never reads (precompiled .cwasm, static archives). */
export function pruneTree(x) {
  walk(x, (p) => { if (/\.(cwasm|a)$/.test(p)) rmSync(p); });
}

/**
 * The bundle split: every file of the tree that READ (guest paths) does not
 * name leaves it for STORE, once by content (named by its hash). A file with
 * the same bytes as one the run read stays too: zstd's window stores it as a
 * match, so it costs the download nothing, and a read the recording missed
 * (Paint opens WinSxS's copy of gdiplus.dll in some runs) is not a lazy fetch
 * of the whole raw file. Returns the lazy files, `[path, size, hash, mtime]`
 * (the time for `mtimes=1`: wineboot compares wine.inf's with the prefix's).
 */
export function splitTree(x, read, store) {
  mkdirSync(store, { recursive: true });
  const lazy = [];
  const kept = new Set();
  walk(x, (p, rel) => { if (lstatSync(p).isFile() && read.has(`/${rel}`)) kept.add(sha(readFileSync(p))); });
  walk(x, (p, rel) => {
    const st = lstatSync(p);
    if (!st.isFile() || read.has(`/${rel}`)) return;
    const b = readFileSync(p);
    const h = sha(b);
    if (kept.has(h)) return;
    if (!existsSync(join(store, h))) writeFileSync(join(store, h), b);
    lazy.push([`/${rel}`, st.size, h, Math.trunc(st.mtimeMs / 1000)]);
    rmSync(p);
  });
  return lazy;
}

/** eject-site.mjs --rest-paths lines: `PATH`, or `PATH HEAD` (HEAD a
 *  whole number of MiB), as [path, head bytes or 0]. */
export const restPaths = (lines) => lines.map((l) => {
  const m = /^(.*\S)\s+(\d+)$/.exec(l);
  return m ? [m[1], Number(m[2]) * 1048576] : [l, 0];
});

/** The lazy files' heads (`heads`, guest path -> bytes) back in the tree
 *  `x` at their paths, each the first bytes of its lazy copy in `store`,
 *  with its time: a lazy file of the rest bundle whose start the boot reads
 *  (the title's music) has that part in the bundle too, at its first-read
 *  place, and the kernel holds it as the file's first chunks (tmpfs.rs
 *  `seed_head`). A file no longer than its head has none (it is dropped from
 *  `heads`). */
export function placeHeads(x, lazy, store, heads) {
  for (const [p, size, h, mtime] of lazy) {
    const n = heads.get(p);
    if (n === undefined) continue;
    if (n >= size) { heads.delete(p); continue; }
    const dst = join(x, p.slice(1));
    writeFileSync(dst, readFileSync(join(store, h)).subarray(0, n));
    utimesSync(dst, mtime, mtime);
    console.log(`eject-site: ${p}: its first ${n / 1048576} MiB in the bundle as its head`);
  }
  for (const p of heads.keys()) if (!lazy.some((f) => f[0] === p)) heads.delete(p);
}

/** The tree as the tar TAR again: directories first, then the files, with
 *  `dedupe` identical files next to each other (zstd's window stores the
 *  second as a match). `order` (guest paths): those files first, in that
 *  order (a recorded run's first reads), then the rest as before. */
export function pack({ x, readOnly, dirTimes }, tar, work, dedupe, order = null) {
  const dirs = [];
  const byHash = new Map();
  const all = (d, rel) => {
    for (const e of readdirSync(d, { withFileTypes: true })) {
      const r = rel ? `${rel}/${e.name}` : e.name;
      const p = join(d, e.name);
      if (e.isDirectory()) { dirs.push(r); all(p, r); continue; }
      const k = dedupe && e.isFile() ? sha(readFileSync(p)) : `path:${r}`;
      if (!byHash.has(k)) byHash.set(k, []);
      byHash.get(k).push(r);
    }
  };
  all(x, '');
  let groups = [...byHash.values()];
  if (order) {
    // A group goes where its first-read member does.
    const rank = new Map();
    order.forEach((p, i) => { if (!rank.has(p)) rank.set(p, i); });
    const at = (g) => Math.min(...g.map((r) => rank.get(`/${r}`) ?? Infinity));
    groups = groups.map((g, i) => [at(g), i, g]).sort((a, b) => a[0] - b[0] || a[1] - b[1]).map((e) => e[2]);
  }
  const list = join(work, 'tar-list');
  writeFileSync(list, [...dirs, ...groups.flat()].join('\n') + '\n');
  for (const [p, mode] of readOnly) chmodSync(p, mode);
  // Deepest first; a directory removed since (none is) is skipped.
  for (const [p, at, mt] of [...dirTimes].reverse()) if (existsSync(p)) utimesSync(p, at, mt);
  execFileSync('tar', ['-C', x, '--no-recursion', '-cf', tar, '-T', list]);
  for (const [p, mode] of readOnly) chmodSync(p, mode | 0o700);
}

/** A unit's image path as a list of guest paths names it: Wine's drive path
 *  (/pfx/dosdevices/c:/...) as the bundle's (/pfx/drive_c/...), lower case. */
export const unitGuest = (p) => String(p ?? '').replace(/^\/pfx\/dosdevices\/c:\//i, '/pfx/drive_c/').toLowerCase();

/** A list of guest paths, one a line, `#` comments (--late, --boot-units). */
export const readPathList = (file) => readFileSync(file, 'utf8').split('\n').map((l) => l.trim()).filter((l) => l && !l.startsWith('#'));

/**
 * The background fill: bytes the page downloads after the boot's, while the
 * guest runs (app.js `fill`; fill.json names where each file starts).
 *
 * - `late`: guest paths of lazy files (`lazy`, splitTree's list, their bytes in
 *   `store`), each followed by its image's units (`unit:KEY/FILE`), which the
 *   page compiles from the fill once their bytes are in;
 * - `boot`: the images whose units the program loads before it is up (a
 *   shell's taskbar). Every other unit follows, in the order the recording
 *   first used them, then the ones it never used: shared as bytes and compiled
 *   when a process first maps one (fill.json's `demand`); a process that maps
 *   one before the fill has it waits for its bytes there (worker.mjs
 *   `loadFile`). A unit that is no image's (gexc eject's "[nt trampolines]",
 *   which every 32-bit process uses from its first system call) stays in the
 *   boot.
 *
 * `units`: the recording's units ({ key, path, at }); `unitDir`: the unit cache
 * (KEY/c.wasm, KEY/c.entries); `lazyUrl(hash)`: a lazy file's name in the
 * fill. Returns { bytes, files: [[name, offset, size]], demand: [KEY], keys:
 * every unit KEY in the fill, lateFiles: how many lazy files }.
 */
export function buildFill({ late = [], lazy = [], store, boot = null, units = [], unitDir, lazyUrl }) {
  const usable = (u) => existsSync(join(unitDir, u.key, 'c.wasm')) && !existsSync(join(unitDir, u.key, 'cold'));
  const byPath = new Map(lazy.map((f) => [f[0], f]));
  const seen = new Set();
  const keys = new Set();
  const demand = [];
  const files = [];
  const parts = [];
  let off = 0;
  const put = (name, bytes) => { files.push([name, off, bytes.length]); parts.push(bytes); off += bytes.length; };
  const putUnit = (u) => { keys.add(u.key); for (const n of ['c.entries', 'c.wasm']) put(`unit:${u.key}/${n}`, readFileSync(join(unitDir, u.key, n))); };
  for (const p of late) {
    const f = byPath.get(p);
    if (f && !seen.has(f[2])) {
      seen.add(f[2]);
      put(lazyUrl(f[2]), readFileSync(join(store, f[2])));
    }
    for (const u of units.filter((u) => unitGuest(u.path) === p.toLowerCase())) if (!keys.has(u.key) && usable(u)) putUnit(u);
  }
  if (boot) {
    const at = (u) => (typeof u.at === 'number' ? u.at : Infinity);
    const deferred = units
      .filter((u) => !String(u.path ?? '').startsWith('[') && !boot.has(unitGuest(u.path)) && !keys.has(u.key) && usable(u))
      .sort((a, b) => at(a) - at(b) || (a.key < b.key ? -1 : 1));
    for (const u of deferred) {
      if (keys.has(u.key)) continue;
      demand.push(u.key);
      putUnit(u);
    }
  }
  return { bytes: Buffer.concat(parts), files, demand, keys, lateFiles: seen.size };
}
