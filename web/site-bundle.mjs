// A W3 program's bundle as a site ships it, for every site builder (web/eject-site.mjs,
// web/page-site.mjs): the tar extracted, its .wasm stripped, what the browser never
// reads pruned, the bundle split (the files the recorded run opened stay in it, the
// rest become lazy files the page reads from the host when a guest first opens one),
// and the tar written again, deduplicated by content.
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { chmodSync, copyFileSync, existsSync, lstatSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
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
 * (`pack`).
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
  return { x, readOnly };
}

/** --strip: every .wasm of the tree stripped. */
export function stripTree(x, stripped) {
  walk(x, (p) => { if (p.endsWith('.wasm')) copyFileSync(stripped(p), p); });
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

/** The tree as the tar TAR again: directories first, then the files, with
 *  `dedupe` identical files next to each other (zstd's window stores the
 *  second as a match). */
export function pack({ x, readOnly }, tar, work, dedupe) {
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
  const list = join(work, 'tar-list');
  writeFileSync(list, [...dirs, ...[...byHash.values()].flat()].join('\n') + '\n');
  for (const [p, mode] of readOnly) chmodSync(p, mode);
  execFileSync('tar', ['-C', x, '--no-recursion', '-cf', tar, '-T', list]);
  for (const [p, mode] of readOnly) chmodSync(p, mode | 0o700);
}
