#!/usr/bin/env node
// An ejected Windows program's web build as a static site, in one command:
//
//   node web/eject-site.mjs PROJECT OUT [--title T] [--bench] [--drop GUEST_PATH]... [--overlay DIR]... [--input SCRIPT] [--args 'A B'] [--wgsl-cache FILE] [--level N]
//
// PROJECT is a built project (`gexc eject ... --web-shim ... --web-page ...`,
// then ./build.sh). OUT gets:
//   index.html     the project's page (user/index.html, the user's), which
//                  imports app.mjs and calls its start()
//   app.mjs, assets/, gex/, vendor/   the app library (runtime/web/page, or
//                  a 32-bit Wine build's build/web/page), runtime and Mesa, plain files
//   project/build/web/nt.json, project/build/web/rootfs.tar.zst, and the
//                  units the browser loads (project/build/units/...)
//   pack.json      every packed file: size, compressed size; the build id
//   _headers       caching and isolation headers (Netlify, Cloudflare Pages)
//   serve.mjs      a static server with the same headers (node serve.mjs [port])
//   README.txt     hosting notes
// Every file of the bundle over 64 KiB, and the units' over 4 KiB, is zstd-compressed (-19
// --long=27, cached beside OUT in OUT.zcache by content) as FILE.zst, which
// the library fetches and decodes itself (app.js, pack.json). No service
// worker: the host sends the isolation headers (serve.mjs, _headers). --bench keeps the recorded benchmark settings
// (scripted input, the 4x clock); without it they are dropped, so a player
// gets the real clock and their own input. --drop removes a guest path from
// the bundle (e.g. a benchmark's seed-pinning mod). --late FILE and
// --boot-units FILE: what goes in the background fill (below).

import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { chmodSync, copyFileSync, cpSync, existsSync, lstatSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, truncateSync, utimesSync, writeFileSync } from 'node:fs';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';
import { buildFill, extract, pack, pruneTree, readPathList, splitTree, stripper, stripTree, unitGuest, walk } from './site-bundle.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const av = process.argv.slice(2);
const flag = (n) => av.includes(n);
const opt = (n, d) => { const i = av.indexOf(n); return i >= 0 ? av[i + 1] : d; };
const opts = (n) => av.flatMap((a, i) => (a === n ? [av[i + 1]] : []));
const [project, out] = av.filter((a, i) => !a.startsWith('--') && !['--title', '--drop', '--overlay', '--level', '--input', '--args', '--wgsl-cache', '--late', '--boot-units', '--stable-from'].includes(av[i - 1]));
// A 32-bit Wine page's site (below): --strip takes the debug sections out of every
// .wasm (the bundle's included), --prune leaves out files the browser never
// reads (precompiled .cwasm, static archives), --dedupe puts identical files
// next to each other in the tar so zstd's window stores the second as a match.
// --pieces: the bundle's file bytes and the units in content-addressed
// pieces (`pieces` in wine32Site), which the page keeps in its own cache, so a
// visit after a rebuild fetches only the pieces that changed; --stable-from
// DIR (repeatable: the Windows image the program's files came from, a game's
// own directory) keeps the files whose bytes are DIR's in pieces of their
// own, which a rebuild of the rest never changes.
if (!project || !out) {
  console.error('usage: node web/eject-site.mjs PROJECT OUT [--title T] [--bench] [--drop GUEST_PATH]... [--overlay DIR]... [--input SCRIPT] [--args "A B"] [--wgsl-cache FILE] [--level N]');
  process.exit(2);
}
const level = opt('--level', '19');
// A 32-bit Wine page's project (gexc eject of a tools/eject/record-wine32.mjs recording):
// build/web/wine32.json instead of nt.json. See `wine32Site` below.
const w3 = existsSync(join(project, 'build/web/wine32.json')) ? JSON.parse(readFileSync(join(project, 'build/web/wine32.json'), 'utf8')) : null;
const nt = w3 ? null : JSON.parse(readFileSync(join(project, 'build/web/nt.json'), 'utf8'));
const title = opt('--title', w3 ? (w3.title ?? 'gex').replace(/^Untitled - /, '') : nt.exe.split('/').pop().replace(/\.exe$/i, ''));
const zcache = `${out.replace(/\/+$/, '')}.zcache`;
rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });
mkdirSync(zcache, { recursive: true });

const files = {}; // site path -> { size, zsize }
const sha = (b) => createHash('sha256').update(b).digest('hex');

/** Put `src` at site path `path`: compressed (FILE.zst) when large.
 *  `plain` keeps the file itself beside its .zst, for a reader that does not
 *  go through the pack. */
function place(src, path, { plain = false } = {}) {
  const dst = join(out, path);
  mkdirSync(dirname(dst), { recursive: true });
  const size = statSync(src).size;
  // A unit's files are fetched one by one whatever their size, and wasm and
  // entry lists halve or better under zstd: packed from 4 KiB.
  if (size < (/\/units\//.test(path) ? 4096 : 65536)) {
    copyFileSync(src, dst);
    return;
  }
  const bytes = readFileSync(src);
  const z = join(zcache, `${sha(bytes)}-${level}.zst`);
  if (!existsSync(z)) execFileSync('zstd', [`-${level}`, '--long=27', '-T4', '-q', '-f', src, '-o', z], { stdio: 'inherit' });
  copyFileSync(z, `${dst}.zst`);
  if (plain) copyFileSync(src, dst);
  // `h`: the content, by which the page keeps the file across builds.
  files[`/${path}`] = { size, zsize: statSync(z).size, h: sha(bytes).slice(0, 16) };
}

/** A split unit's cold clusters (`gexc eject build` with "cold": DIR/N.wasm)
 *  at site path `path`: `cold.bin`, each cluster its own zstd frame, which
 *  the page fetches by Range on the cluster's first call (gex.mjs `gex.cold`),
 *  and `cold.idx`, the u32 length of its index, then the index as a zstd
 *  frame: u32 n, the n frames' lengths, the n clusters' unpacked lengths. */
function coldPack(dir, path) {
  const n = readdirSync(dir).filter((f) => /^\d+\.wasm$/.test(f)).length;
  const z = join(`${out.replace(/\/+$/, '')}.work`, 'cold', path);
  rmSync(z, { recursive: true, force: true });
  mkdirSync(z, { recursive: true });
  for (let i = 0; i < n; i++) copyFileSync(join(dir, `${i}.wasm`), join(z, `${i}.wasm`));
  // Thousands of small frames: many zstd processes at once, each a batch.
  execFileSync('sh', ['-c', `ls | grep '\\.wasm$' | xargs -P 16 -n 256 zstd -${level} -q -f`], { cwd: z });
  const frames = [];
  const zl = new Uint32Array(n), ul = new Uint32Array(n);
  for (let i = 0; i < n; i++) {
    const f = readFileSync(join(z, `${i}.wasm.zst`));
    frames.push(f);
    zl[i] = f.length;
    ul[i] = statSync(join(z, `${i}.wasm`)).size;
  }
  mkdirSync(join(out, path), { recursive: true });
  writeFileSync(join(out, path, 'cold.bin'), Buffer.concat(frames));
  const raw = Buffer.concat([Buffer.from(new Uint32Array([n]).buffer), Buffer.from(zl.buffer), Buffer.from(ul.buffer)]);
  const zi = execFileSync('zstd', [`-${level}`, '-q', '-c'], { input: raw });
  const head = Buffer.alloc(4);
  head.writeUInt32LE(raw.length);
  writeFileSync(join(out, path, 'cold.idx'), Buffer.concat([head, zi]));
  rmSync(z, { recursive: true, force: true });
}

/** How many pieces each class of the bundle's files (stable, the rest) is
 *  cut into, and the units: a file's piece is its path's hash modulo this,
 *  so a changed file changes its own piece and no other. */
const PIECES = 16;

if (w3) {
  wine32Site();
  process.exit(0);
}

/** Put `src` at site path `path` as it is (what the page loads by URL). */
function plain(src, path) {
  const dst = join(out, path);
  mkdirSync(dirname(dst), { recursive: true });
  copyFileSync(src, dst);
}

// The page and the runtime, plain: nothing between the page and the server
// decodes (a host's own compression still applies).
walk(join(project, 'runtime/web/page'), (p, rel) => plain(p, rel));
// --wgsl-cache FILE: Mesa's shader translations from a training run
// (play.mjs's `wgsl` step), beside the Mesa glue, where mesa-gl.js looks.
// Mesa refuses a blob another build of it made, so it goes stale with Mesa.
if (opt('--wgsl-cache', null)) plain(opt('--wgsl-cache', null), 'gex/gex-wgsl-cache.bin');

// The units the browser loads: each unit's module and entries, region
// entries, providers, stateful providers, indirect-call profiles (part of a
// unit's key) and the link groups' index. Not the native build's (.cwasm,
// objects, recovery, precompiled).
const cgen = nt.cgen;
const units = join(project, cgen);
let unitBytes = 0;
const preload = [];
// Only this build's units: a unit cache keeps every key it ever built
// (a re-keyed rebuild leaves the old ones). build/build.json names the
// images' units; the NT trampolines' unit is not an image, and is kept when
// it was linked by this build (no older than the oldest image unit).
const built = JSON.parse(readFileSync(join(project, 'build/build.json'), 'utf8'));
const ids = new Set(built.images.map((i) => i.identity).filter(Boolean));
const since = Math.min(...[...ids].map((k) => statSync(join(units, k, 'c.wasm')).mtimeMs)) - 60000;
// The NT trampolines' unit is no image's: nt.json names it (older builds
// did not, and fall back to the age test).
if (nt.trampolines) ids.add(nt.trampolines);
const current = (key) => ids.has(key) || (existsSync(join(units, key, 'c.wasm')) && statSync(join(units, key, 'c.wasm')).mtimeMs >= since);
let stale = 0;
walk(units, (p, rel) => {
  const parts = rel.split('/');
  const top = parts[0];
  if (['objects', 'recovery', 'precompiled'].includes(top)) return;
  const key = top === 'regions' ? parts[1] : parts.length === 2 && /^[0-9a-f]{64}/.test(top) ? top : null;
  if (key && !current(key)) { stale++; return; }
  if (rel.includes('/recovery/')) return;
  const name = rel.split('/').pop();
  const want = name === 'c.wasm' || name === 'c.entries'
    || (top === 'providers' && name.endsWith('.wasm'))
    || top === 'stateful'
    || (top === 'icall' && name.endsWith('.txt'))
    || (top === 'groups' && name === 'index.txt');
  if (!want) return;
  unitBytes += statSync(p).size;
  place(p, join('project', cgen, rel));
  // What the library compiles while the bundle downloads (its preload list):
  // module names as gex.mjs keys them.
  const url = `/project/${cgen}/${rel}`;
  if (name === 'c.wasm') preload.push([`cgen:${rel.slice(0, -'/c.wasm'.length)}`, url]);
  else if (top === 'providers') preload.push([`cgenfile:${rel}`, url]);
  // Every other file a Worker reads from the unit cache with a synchronous
  // request (entries, providers, Lua, icall profiles, the groups' index):
  // fetched by the page ahead and handed to every Worker.
  if (name !== 'c.wasm') preload.push([`file:${rel}`, url]);
});

// Largest first: a game's exe unit is often most of the compile.
preload.sort((a, b) => (files[b[1]]?.size ?? 0) - (files[a[1]]?.size ?? 0));
writeFileSync(join(out, 'project', cgen, 'preload.json'), JSON.stringify(preload));

// The bundle, without what --drop names, with --overlay DIR's files (guest
// paths under DIR, as user/files) laid over it.
const drops = opts('--drop');
const overlays = opts('--overlay');
let tar = join(project, 'build/web/rootfs.tar');
if (drops.length || overlays.length) {
  tar = join(zcache, 'rootfs.tar');
  execFileSync('tar', ['-C', join(project, 'build/rootfs'), '-chf', tar, ...drops.map((d) => `--exclude=.${d}`), '.']);
  for (const d of overlays) execFileSync('tar', ['-C', d, '-rhf', tar, '.']);
}
place(tar, 'project/build/web/rootfs.tar');

// How to start: the recorded settings, less the benchmark's.
const bench = /^(GEX_WIN_INPUT|GEX_WIN_TIME_SCALE|GEX_WIN_FIXED_TIME)=/;
let settings = flag('--bench') ? nt.settings : nt.settings.filter((s) => !bench.test(s));
// --input SCRIPT: GEX_WIN_INPUT instead of the recorded one (a benchmark's
// clicks, by presented frame).
if (opt('--input', null)) settings = [...settings.filter((s) => !s.startsWith('GEX_WIN_INPUT=')), `GEX_WIN_INPUT=${opt('--input', null)}`];
// --args 'A B': the program's arguments instead of the recorded ones ('' for none).
const args = av.includes('--args') ? opt('--args', '').split(' ').filter(Boolean) : nt.args;
// Direct present for a GL program (the page composites again whenever a
// second window or one without GL appears); --no-direct: always composite.
const ntOut = { ...nt, settings, args, direct: !!nt.gl && !flag('--no-direct') };
mkdirSync(join(out, 'project/build/web'), { recursive: true });
writeFileSync(join(out, 'project/build/web/nt.json'), JSON.stringify(ntOut, null, 2));

// The page (the user's). The build: everything served, packed or not (a
// page-only change is a new build too).
page();
const smallHash = createHash('sha256');
walk(out, (p, rel) => { smallHash.update(rel); smallHash.update(readFileSync(p)); });
const build = sha(JSON.stringify(files) + JSON.stringify(ntOut) + smallHash.digest('hex')).slice(0, 12);
writeFileSync(join(out, 'pack.json'), JSON.stringify({ build, files }, null, 1));

writeHosting();
noTestPage();

const total = Object.values(files).reduce((a, f) => a + f.size, 0);
const ztotal = Object.values(files).reduce((a, f) => a + f.zsize, 0);
let small = 0;
walk(out, (p) => { if (!p.endsWith('.zst')) small += statSync(p).size; });
writeFileSync(join(out, 'README.txt'), `${title} in the browser (an ejected Windows program on gex), build ${build}

Serve this directory as static files at the root of an HTTPS origin (or
http://localhost): node serve.mjs [port] does, with the headers below.

Browser: a desktop Chrome or Edge with WebGPU and hardware acceleration.
Download: ${mb(ztotal + small)} MB (${mb(total)} MB unpacked), cached by the browser after
the first load; saves stay in this browser's IndexedDB.

Headers (_headers has them for Netlify and Cloudflare Pages):
  every file:  Cross-Origin-Opener-Policy: same-origin
               Cross-Origin-Embedder-Policy: require-corp
  *.zst and assets/*: Cache-Control: public, max-age=31536000, immutable
  the rest:    Cache-Control: no-cache
The isolation headers are required (threads need them); the launcher says so
when they are missing.

The .zst files are decoded by the page; serve them as plain bytes, with no
Content-Encoding. nginx:
  location / {
    add_header Cross-Origin-Opener-Policy same-origin always;
    add_header Cross-Origin-Embedder-Policy require-corp always;
    types { application/javascript mjs js; application/wasm wasm; application/json json; text/html html; }
  }
`);
/** _headers and serve.mjs: the headers the site wants from its host. */
function writeHosting() {
  // Headers: isolation everywhere; packed files and hashed assets cached for
  // good (their URLs carry the build); the rest revalidated.
  writeFileSync(join(out, '_headers'), `/*
  Cross-Origin-Opener-Policy: same-origin
  Cross-Origin-Embedder-Policy: require-corp
  Cross-Origin-Resource-Policy: same-origin
  Cache-Control: no-cache
/*.zst
  Cache-Control: public, max-age=31536000, immutable
/assets/*
  Cache-Control: public, max-age=31536000, immutable
/project/build/web/lazy/*
  Cache-Control: public, max-age=31536000, immutable
`);
  writeFileSync(join(out, 'serve.mjs'), `#!/usr/bin/env node
// A static server for this site with the headers it wants (_headers). node serve.mjs [port]
import { createServer } from 'node:http';
import { createReadStream, statSync } from 'node:fs';
import { dirname, extname, join, normalize } from 'node:path';
import { fileURLToPath } from 'node:url';
const here = dirname(fileURLToPath(import.meta.url));
const port = Number(process.argv[2] ?? 8000);
const types = { '.html': 'text/html', '.mjs': 'text/javascript', '.js': 'text/javascript', '.json': 'application/json', '.wasm': 'application/wasm', '.css': 'text/css', '.png': 'image/png' };
createServer((req, res) => {
  const path = normalize(decodeURIComponent(new URL(req.url, 'http://x').pathname));
  if (path.includes('..')) { res.writeHead(403).end(); return; }
  const file = join(here, path.endsWith('/') ? path + 'index.html' : path);
  let st; try { st = statSync(file); } catch { st = null; }
  if (!st?.isFile()) { res.writeHead(404).end(); return; }
  const immutable = path.endsWith('.zst') || path.startsWith('/assets/') || path.startsWith('/project/build/web/lazy/');
  const h = {
    'Content-Type': types[extname(file)] ?? 'application/octet-stream', 'Accept-Ranges': 'bytes',
    'Cross-Origin-Opener-Policy': 'same-origin', 'Cross-Origin-Embedder-Policy': 'require-corp', 'Cross-Origin-Resource-Policy': 'same-origin',
    'Cache-Control': immutable ? 'public, max-age=31536000, immutable' : 'no-cache',
  };
  // Ranges: a lazy file is read 1 MiB at a time (a bundle split's).
  const r = /^bytes=(\\d+)-(\\d*)$/.exec(req.headers.range ?? '');
  if (r) {
    const a = Number(r[1]), b = Math.min(r[2] ? Number(r[2]) : st.size - 1, st.size - 1);
    if (a > b) { res.writeHead(416, { ...h, 'Content-Range': \`bytes */\${st.size}\` }).end(); return; }
    res.writeHead(206, { ...h, 'Content-Length': b - a + 1, 'Content-Range': \`bytes \${a}-\${b}/\${st.size}\` });
    if (req.method === 'HEAD') { res.end(); return; }
    createReadStream(file, { start: a, end: b }).pipe(res);
    return;
  }
  res.writeHead(200, { ...h, 'Content-Length': st.size });
  if (req.method === 'HEAD') { res.end(); return; }
  createReadStream(file).pipe(res);
}).listen(port, () => console.log(\`http://localhost:\${port}/\`));
`);
}
function mb(n) { return (n / 1048576).toFixed(1); }

/** The project's page, user/index.html (gexc eject writes an example once;
 *  it is the user's), at the site's root. */
function page() {
  const src = join(project, 'user/index.html');
  if (!existsSync(src)) {
    console.error(`eject-site: ${src} is missing: the page is the project's own (gexc eject writes an example; re-eject to get it)`);
    process.exit(1);
  }
  copyFileSync(src, join(out, 'index.html'));
  if (!existsSync(join(out, 'app.mjs'))) {
    console.error('eject-site: the build has no app.mjs (the app library: web/build-eject-page.sh)');
    process.exit(1);
  }
}

/** gex's test page (nt.html) is never part of an ejected site. */
function noTestPage() {
  const bad = [];
  walk(out, (p, rel) => {
    if (rel.startsWith('project/build/web/lazy/')) return;
    if (rel.split('/').pop() === 'nt.html') bad.push(rel);
    else if (/\.(m?js|html|json)$/.test(rel) && readFileSync(p).includes('nt.html')) bad.push(rel);
  });
  if (bad.length) {
    console.error(`eject-site: the site has or names nt.html (gex's test page): ${bad.slice(0, 5).join(', ')}`);
    process.exit(1);
  }
}

/** The files of DIRs (--stable-from), for `pieces`: by lower-case name
 *  and size, the paths with those. */
function stableIndex(dirs) {
  const by = new Map();
  const add = (d) => {
    let es;
    try { es = readdirSync(d, { withFileTypes: true }); } catch { return; }
    for (const e of es) {
      const p = join(d, e.name);
      if (e.isDirectory()) add(p);
      else if (e.isFile()) {
        const k = `${e.name.toLowerCase()}:${statSync(p).size}`;
        if (!by.has(k)) by.set(k, []);
        by.get(k).push(p);
      }
    }
  };
  for (const d of dirs) add(d);
  return by;
}

/**
 * The bundle's file bytes as content-addressed pieces (--pieces): every
 * regular file of the extracted bundle X goes into one of PIECES pieces of
 * its class, by its path's hash, and is left in X empty, so the tar keeps
 * its name, mode and time (which change from build to build: a staged file
 * is copied anew) and no bytes. A piece is its files' bytes one after
 * another, in path order, named by their hash: the same files give the same
 * piece, whatever else was rebuilt. The class "stable" is the files whose
 * bytes are a file of --stable-from's, by name and size (a Windows DLL, as
 * the image has it); the rest (Wine, the prefix, the runtime's tree) are
 * cut apart from them, so a rebuild of the rest never touches a stable
 * piece. Returns the index the page reads (pieces.json): the pieces, by
 * URL and size, and each file's piece, offset and length.
 */
function pieces(x) {
  const work = join(`${out.replace(/\/+$/, '')}.work`, 'pieces');
  rmSync(work, { recursive: true, force: true });
  mkdirSync(work, { recursive: true });
  const stable = stableIndex(opts('--stable-from'));
  const isStable = (p, name, size, bytes) => {
    for (const c of stable.get(`${name.toLowerCase()}:${size}`) ?? []) if (readFileSync(c).equals(bytes)) return true;
    return false;
  };
  const groups = new Map(); // "class/n" -> [[rel, bytes]]
  // A file's other names (hard links, which --dedupe makes): emptied with
  // the first, so each takes the first's bytes and piece.
  const inodes = new Map(); // "dev:ino" -> [rel, bytes]
  // Each file with its bytes and class, then its group: the group of the
  // first path (in order) of its class with the same bytes, so a file's
  // copies (Wine's builtin in /w3 and its copy in system32, the four of each
  // .nls) are stored once, in one piece. By its own path they fell in
  // different pieces, and zstd, a piece at a time, stored each: 22.5 MB of
  // a desktop shell's 479 MB, 7.7 MB packed. Still a function of the files
  // alone: the same files give the same pieces.
  const found = []; // [rel, bytes, class, hash]
  walk(x, (p, rel) => {
    const st = lstatSync(p);
    if (!st.isFile()) return;
    const ino = `${st.dev}:${st.ino}`;
    if (inodes.has(ino)) {
      const [first, b] = inodes.get(ino);
      found.push([rel, b, null, first]);
      return;
    }
    if (!st.size) return;
    const b = readFileSync(p);
    const cls = stable.size && isStable(p, rel.split('/').pop(), st.size, b) ? 'stable' : 'rest';
    found.push([rel, b, cls, sha(b)]);
    if (st.nlink > 1) inodes.set(ino, [rel, b]);
    // Emptied, its mode and time kept (a read-only file made writable for it).
    if (!(st.mode & 0o200)) chmodSync(p, st.mode | 0o200);
    truncateSync(p, 0);
    if (!(st.mode & 0o200)) chmodSync(p, st.mode & 0o7777);
    utimesSync(p, st.atime, st.mtime);
  });
  const firstOf = new Map(); // "class:hash" -> the first path with them
  for (const [rel, , cls, h] of found) {
    if (cls === null) continue;
    const k = `${cls}:${h}`;
    if (!firstOf.has(k) || rel < firstOf.get(k)) firstOf.set(k, rel);
  }
  const groupOf = new Map(); // rel -> group
  for (const [rel, b, cls, h] of found) {
    if (cls === null) continue;
    const g = `${cls}/${parseInt(sha(firstOf.get(`${cls}:${h}`)).slice(0, 8), 16) % PIECES}`;
    groupOf.set(rel, g);
    if (!groups.has(g)) groups.set(g, []);
    groups.get(g).push([rel, b]);
  }
  // A hard link: with the first name's bytes, in its piece.
  for (const [rel, b, cls, first] of found) if (cls === null) groups.get(groupOf.get(first)).push([rel, b]);
  const index = { pieces: [], files: [] };
  // The stable files' guest paths (lower case), for the units' grouping;
  // not written out.
  Object.defineProperty(index, 'stable', { value: new Set(), enumerable: false });
  // Whether a file elsewhere (a lazy file, for its unit) is a stable one.
  Object.defineProperty(index, 'isStable', { value: (f, name) => {
    if (!stable.size || !existsSync(f)) return false;
    const b = readFileSync(f);
    return isStable(f, name, b.length, b);
  }, enumerable: false });
  let bytes = 0, stableBytes = 0;
  for (const g of [...groups.keys()].sort()) {
    const list = groups.get(g).sort((a, b) => (a[0] < b[0] ? -1 : 1));
    const parts = [];
    const at = new Map(); // a piece holds the same bytes once
    let off = 0;
    const files = [];
    for (const [rel, b] of list) {
      const h = sha(b);
      if (!at.has(h)) { at.set(h, off); parts.push(b); off += b.length; }
      files.push([`/${rel}`, at.get(h), b.length]);
    }
    const body = Buffer.concat(parts);
    const id = sha(body).slice(0, 32);
    const f = join(work, id);
    writeFileSync(f, body);
    place(f, `project/build/web/pieces/${id}`);
    const i = index.pieces.length;
    index.pieces.push([`/project/build/web/pieces/${id}`, body.length, g.split('/')[0]]);
    for (const [path, o, n] of files) {
      index.files.push([path, i, o, n]);
      if (g.startsWith('stable/')) index.stable.add(path.toLowerCase());
    }
    bytes += body.length;
    if (g.startsWith('stable/')) stableBytes += body.length;
  }
  console.log(`eject-site: pieces: ${index.pieces.length} (${index.files.length} files, ${mb(bytes)} MB; stable ${mb(stableBytes)} MB)`);
  return index;
}

/** The site of a 32-bit Wine page's project: its page (built with the project's
 *  features, as plain files), its bundle and units (packed, decoded by the
 *  page), the launcher and the hosting files. No service worker. */
function wine32Site() {
  const web = join(project, 'build/web');
  const work = `${out.replace(/\/+$/, '')}.work`;
  rmSync(work, { recursive: true, force: true });
  mkdirSync(work, { recursive: true });
  const strip = flag('--strip');
  const stripped = stripper(work, strip);
  // The page as plain files: nothing between it and the server decodes, so
  // only what the page itself fetches and decodes (the bundle, the units:
  // app.js, pack.json) is packed. A host's own compression still applies.
  walk(join(web, 'page'), (p, rel) => {
    // The runtime's wasm (shim, seed, kernel) is fetched by the page itself
    // (app.js primes runtime.js with it): packed, as the units are.
    if (/^gex\/(gex_browser|gex_kernel|seed)[^/]*\.wasm$/.test(rel)) {
      place(stripped(p), rel);
      return;
    }
    // Mesa (the page compositor's GL, app.js `mesaSource`): packed, and
    // plain as well for the scene compositor's Worker, whose glue fetches its
    // files itself. Not stripped: the split's two halves are one program.
    if (/^gex\/gex-wgpu-replay(\.deferred)?\.(wasm|js)$/.test(rel)) {
      place(p, rel, { plain: true });
      return;
    }
    const dst = join(out, rel);
    mkdirSync(dirname(dst), { recursive: true });
    copyFileSync(stripped(p), dst);
  });
  // The bundle.
  let tar = join(web, 'rootfs.tar');
  // `"bundle": "split"` (user/eject.json): the files the recorded run opened
  // stay in the bundle; every other file is a lazy file (app.js `lazy`),
  // stored once by content under project/build/web/lazy/ and read from the
  // host, in 1 MiB ranges, when a guest first reads it.
  const split = w3.bundle?.split ? new Set(w3.bundle.read ?? []) : null;
  // --late FILE: see the fill below.
  const late = opt('--late', null) ? readPathList(opt('--late', null)) : [];
  if (split) for (const p of late) split.delete(p);
  // The late files' units (in the fill, not units.tar).
  let lateKeys = new Set();
  // --boot-units FILE: the images (guest paths, one a line) whose units the
  // program loads before it is up (a shell's taskbar), recorded from a run of
  // the page. Every other unit goes to the background fill, after the boot's
  // download, not in it (site-bundle.mjs `buildFill`): the shell's bundled
  // programs' units, 58 MB of its 285 MB first download, which nothing maps
  // before the taskbar.
  const bootUnitsList = opt('--boot-units', null) ? new Set(readPathList(opt('--boot-units', null)).map(unitGuest)) : null;
  const fillDemand = [];
  let lazyFiles = 0, lazyBytes = 0;
  let lazySource = new Map(); // a lazy file's guest path (lower case) -> its bytes in the store
  const piecesOn = flag('--pieces');
  let piecesIndex = null;
  if (strip || flag('--prune') || flag('--dedupe') || split || piecesOn) {
    const tree = extract(tar, work);
    const { x } = tree;
    if (strip) stripTree(x, stripped);
    if (flag('--prune')) pruneTree(x);
    if (split) {
      split.add(w3.exe); // the page reads the loader from the bundle itself
      const store = join(out, 'project/build/web/lazy');
      const lazy = splitTree(x, split, store);
      lazyFiles = lazy.length;
      lazySource = new Map(lazy.map((f) => [f[0].toLowerCase(), join(store, f[2])]));
      lazyBytes = lazy.reduce((a, f) => a + f[1], 0);
      writeFileSync(join(out, 'project/build/web/lazy.json'), JSON.stringify({ base: '/project/build/web/lazy/', files: lazy }));
      w3.params = { ...(w3.params ?? {}), lazy: '/project/build/web/lazy.json' };
      // --late FILE (guest paths, one a line, in the order wanted): files of
      // the run's read set that left the bundle above for the background fill,
      // fill.bin (packed): each lazy file's bytes once, in that order, which
      // the page downloads after the bundle while the guest runs (app.js
      // `fill`); fill.json says where each starts.
      // Each late image's unit follows its bytes there (`unit:KEY/FILE`):
      // the page compiles it from the fill, and a Worker started before
      // that reads it there (app.js, worker.mjs `loadFile`).
      if (late.length || bootUnitsList) {
        const fl = buildFill({ late, lazy, store, boot: bootUnitsList, units: w3.units ?? [], unitDir: join(web, 'units'), lazyUrl: (h) => `/project/build/web/lazy/${h}` });
        lateKeys = fl.keys;
        fillDemand.push(...fl.demand);
        if (bootUnitsList) console.log(`eject-site: --boot-units: ${fillDemand.length} units in the fill, compiled when first mapped`);
        const entries = fl.files, off = fl.bytes.length, fillBytes = fl.bytes;
        const fb = join(work, 'fill.bin');
        writeFileSync(fb, fillBytes);
        // With --pieces, named by its bytes like a piece, which the page keeps
        // in its cache (fill-worker.js): a later visit, or a build that left
        // the fill alone, does not download it again.
        const fillSite = piecesOn ? `project/build/web/pieces/f${sha(fillBytes).slice(0, 32)}.bin` : 'project/build/web/fill.bin';
        place(fb, fillSite);
        writeFileSync(join(out, 'project/build/web/fill.json'), JSON.stringify({ url: `/${fillSite}`, size: off, files: entries, ...(fillDemand.length ? { demand: fillDemand } : {}) }));
        w3.params.fill = '/project/build/web/fill.json';
        console.log(`eject-site: the fill: ${fl.lateFiles} files and ${lateKeys.size} units, ${off} bytes, after the bundle`);
      }
    }
    tar = join(work, 'rootfs.tar');
    if (piecesOn) piecesIndex = pieces(x);
    pack(tree, tar, work, flag('--dedupe'));
  }
  // What the split leaves in the bundle (the run's read set) is all the
  // download has of it; nt.json carries the rest's manifest by URL.
  if (w3.bundle) w3.bundle = { split: !!split, ...(split ? { read: split.size, lazy: lazyFiles, lazy_bytes: lazyBytes } : {}) };
  place(tar, 'project/build/web/rootfs.tar');
  if (piecesIndex) {
    writeFileSync(join(out, 'project/build/web/pieces.json'), JSON.stringify(piecesIndex));
    w3.params = { ...(w3.params ?? {}), pieces: '/project/build/web/pieces.json' };
  }
  // The units. Whole units (no "cold" split, the default): one archive,
  // units.tar, every unit's c.wasm and c.entries, packed as one zstd file.
  // The page decodes it once, compiles every module while the bundle
  // downloads, and asks the host for no other unit file: a Worker's lookup
  // of anything the archive does not hold is a miss, not a request
  // (preload.json `tar:N`, N the number of modules in it).
  const preload = [];
  const coldUnits = w3.units.filter((u) => existsSync(join(web, 'units', u.key, 'cold'))).length;
  // The provider modules the units bind (gexc eject: units/providers/),
  // which the Workers read as unit-cache files.
  const providers = existsSync(join(web, 'units', 'providers'))
    ? readdirSync(join(web, 'units', 'providers')).filter((n) => n.endsWith('.wasm')).sort().map((n) => `providers/${n}`)
    : [];
  // The units' region entries (gexc eject: units/regions/KEY/), modules of
  // their own the page publishes after each unit.
  const regions = existsSync(join(web, 'units', 'regions'))
    ? readdirSync(join(web, 'units', 'regions')).sort().map((k) => `regions/${k}`)
    : [];
  if (!coldUnits) {
    // --late: the late files' units travel in the fill (above), not here.
    const bootUnits = w3.units.filter((u) => !lateKeys.has(u.key));
    // One archive (units.tar), or with --pieces one per group of units, by
    // their image's path (as the bundle's pieces) and its class (a stable
    // file's units apart from the rest's), each named by its bytes: a unit
    // that did not change keeps its archive, which the page has.
    const groupsU = new Map();
    // A unit's image by its Wine path (/pfx/dosdevices/c:/...), the bundle's
    // file by the drive's (/pfx/drive_c/...).
    // A lazy file's unit is stable when the file is.
    const stableU = (u) => {
      if (!piecesIndex) return false;
      const p = String(u.path ?? '').replace(/\/dosdevices\/c:\//i, '/drive_c/').toLowerCase();
      return piecesIndex.stable.has(p) || (lazySource.has(p) && piecesIndex.isStable(lazySource.get(p), p.split('/').pop()));
    };
    for (const u of bootUnits) {
      const g = piecesOn ? (stableU(u) ? 0 : PIECES) + parseInt(sha(String(u.path ?? u.key)).slice(0, 8), 16) % PIECES : 0;
      if (!groupsU.has(g)) groupsU.set(g, []);
      groupsU.get(g).push(u);
    }
    const unitTars = [];
    // Stateful providers (user/eject.json "providers", wine32.json `stateful`):
    // files of the unit cache, in the first archive, with the provider
    // modules and the region entries.
    const statefulFiles = existsSync(join(web, 'units', 'stateful')) ? readdirSync(join(web, 'units', 'stateful')).map((f) => `stateful/${f}`) : [];
    if (!groupsU.size && (statefulFiles.length || providers.length || regions.length)) groupsU.set(0, []);
    let firstGroup = true;
    let first = true;
    for (const g of [...groupsU.keys()].sort((a, b) => a - b)) {
      const us = groupsU.get(g).sort((a, b) => (a.key < b.key ? -1 : 1));
      const list = join(work, 'units-list');
      writeFileSync(list, `${[...us.flatMap((u) => [`${u.key}/c.wasm`, `${u.key}/c.entries`]), ...(first ? [...statefulFiles, ...providers, ...regions.flatMap((r) => [`${r}/c.wasm`, `${r}/c.entries`])] : [])].join('\n')}\n`);
      first = false;
      const ut = join(work, `units-${g}.tar`);
      execFileSync('tar', ['-C', join(web, 'units'), '--dereference', '--owner=0', '--group=0', '--mtime=@0', '--format=gnu', '-cf', ut, '-T', list]);
      const site = piecesOn ? `project/build/web/pieces/u${sha(readFileSync(ut)).slice(0, 32)}.tar` : 'project/build/web/units.tar';
      place(ut, site);
      unitTars.push([`/${site}`, us, firstGroup ? regions.length : 0]);
      firstGroup = false;
    }
    if (piecesOn) console.log(`eject-site: unit pieces: ${unitTars.length} (${bootUnits.filter(stableU).length} of ${bootUnits.length} units stable)`);
    if (lateKeys.size) console.log(`eject-site: ${lateKeys.size} units in the fill (${w3.units.filter((u) => lateKeys.has(u.key)).map((u) => u.path.split('/').pop()).join(' ')})`);
    mkdirSync(join(out, 'project/build/web/units'), { recursive: true });
    // Only the units the recording used by its title are compiled with the
    // page. The rest, first used after it (`at`, the recorder's seconds) or
    // not at all (no `at`: a unit built for every DLL of the prefix), go to
    // the Workers as bytes, and the first process to map one compiles it
    // (gex.mjs `#cgenModule`) and shares it. Compiled, each held 2.8 times
    // its size of the renderer's code space and a copy of its bytes: a
    // shell's bundled programs' units, 170 MiB that no process mapped at start-up,
    // were 640 MiB of the renderer's VmData. A recording that timed no unit
    // (an older recorder) has every unit compiled with the page.
    const title = typeof w3.title_seconds === 'number' ? w3.title_seconds : null;
    const timed = title !== null && bootUnits.some((u) => typeof u.at === 'number');
    const demand = timed ? bootUnits.filter((u) => !(typeof u.at === 'number' && u.at <= title)).map((u) => u.key) : [];
    if (demand.length) console.log(`eject-site: ${demand.length} units compiled when first mapped (${w3.units.filter((u) => demand.includes(u.key)).map((u) => u.path.split('/').pop()).join(' ')})`);
    const demandSet = new Set(demand);
    writeFileSync(join(out, 'project/build/web/units/preload.json'), JSON.stringify(unitTars.map(([url, us, nr]) => {
      const d = us.filter((u) => demandSet.has(u.key)).map((u) => u.key);
      return [`tar:${us.length - d.length + nr}`, url, ...(d.length || nr ? [{ ...(d.length ? { demand: d } : {}), ...(nr ? { regions: nr } : {}) }] : [])];
    })));
  }
  for (const u of coldUnits ? w3.units : []) {
    const d = join(web, 'units', u.key);
    place(join(d, 'c.wasm'), `project/build/web/units/${u.key}/c.wasm`);
    place(join(d, 'c.entries'), `project/build/web/units/${u.key}/c.entries`);
    preload.push([`cgen:${u.key}`, `/project/build/web/units/${u.key}/c.wasm`, statSync(join(d, 'c.wasm')).size]);
    preload.push([`file:${u.key}/c.entries`, `/project/build/web/units/${u.key}/c.entries`, 0]);
    if (existsSync(join(d, 'cold'))) coldPack(join(d, 'cold'), `project/build/web/units/${u.key}`);
  }
  if (coldUnits && existsSync(join(web, 'units', 'stateful'))) {
    for (const f of readdirSync(join(web, 'units', 'stateful'))) {
      place(join(web, 'units', 'stateful', f), `project/build/web/units/stateful/${f}`);
      preload.push([`file:stateful/${f}`, `/project/build/web/units/stateful/${f}`, 0]);
    }
  }
  for (const r of coldUnits ? regions : []) {
    place(join(web, 'units', r, 'c.wasm'), `project/build/web/units/${r}/c.wasm`);
    place(join(web, 'units', r, 'c.entries'), `project/build/web/units/${r}/c.entries`);
    preload.push([`cgen:${r}`, `/project/build/web/units/${r}/c.wasm`, statSync(join(web, 'units', r, 'c.wasm')).size]);
    preload.push([`file:${r}/c.entries`, `/project/build/web/units/${r}/c.entries`, 0]);
  }
  for (const rel of coldUnits ? providers : []) {
    place(join(web, 'units', rel), `project/build/web/units/${rel}`);
    preload.push([`file:${rel}`, `/project/build/web/units/${rel}`, 0]);
  }
  preload.sort((a, b) => b[2] - a[2]);
  if (coldUnits) writeFileSync(join(out, 'project/build/web/units/preload.json'), JSON.stringify(preload.map(([a, b]) => [a, b])));
  writeFileSync(join(out, 'project/build/web/nt.json'), JSON.stringify(w3, null, 2));
  // The page (the user's). No service worker: the host sends the isolation
  // headers (COOP/COEP), and the library decodes what is packed.
  page();
  const smallHash = createHash('sha256');
  // The lazy store is named by its content (lazy.json's hashes).
  walk(out, (p, rel) => { if (rel.startsWith('project/build/web/lazy/')) return; smallHash.update(rel); smallHash.update(readFileSync(p)); });
  const build = sha(JSON.stringify(files) + JSON.stringify(w3) + smallHash.digest('hex')).slice(0, 12);
  writeFileSync(join(out, 'pack.json'), JSON.stringify({ build, files }, null, 1));
  writeHosting();
  noTestPage();
  rmSync(work, { recursive: true, force: true });
  const total = Object.values(files).reduce((a, f) => a + f.size, 0);
  const ztotal = Object.values(files).reduce((a, f) => a + f.zsize, 0);
  let small = 0;
  // Cold clusters are fetched on first call, not with the download.
  let cold = 0;
  walk(out, (p, rel) => {
    if (rel.endsWith('/cold.bin')) cold += statSync(p).size;
    else if (!p.endsWith('.zst') && !rel.startsWith('project/build/web/lazy/')) small += statSync(p).size;
  });
  writeFileSync(join(out, 'site.json'), JSON.stringify({ build, kind: 'w3', features_off: w3.features_off, units: w3.units.length, download: ztotal + small, unpacked: total + small, lazy: split ? { files: lazyFiles, bytes: lazyBytes } : null, cold: cold || null }, null, 1));
  console.log(`${out}: wine32 build ${build}; features off: ${w3.features_off.join(', ') || 'none'}; ${w3.units.length} units; download ${mb(ztotal + small)} MB, unpacked ${mb(total + small)} MB`
    + (split ? `; bundle split: ${split.size} files read by the recorded run in it, ${lazyFiles} lazy (${mb(lazyBytes)} MB, read on demand)` : ''));
}
console.log(`${out}: build ${build}; ${stale} files of superseded units left out; ${Object.keys(files).length} packed files, ${mb(total)} MB -> ${mb(ztotal)} MB, plus ${mb(small)} MB unpacked small files; units ${mb(unitBytes)} MB raw`);
