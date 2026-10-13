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
//                  a W3 build's build/web/page), runtime and Mesa, plain files
//   project/build/web/nt.json, project/build/web/rootfs.tar.zst, and the
//                  units the browser loads (project/build/units/...)
//   pack.json      every packed file: size, compressed size; the build id
//   _headers       caching and isolation headers (Netlify, Cloudflare Pages)
//   serve.mjs      a static server with the same headers (node serve.mjs [port])
//   README.txt     hosting notes
// Every file of the bundle and the units over 64 KiB is zstd-compressed (-19
// --long=27, cached beside OUT in OUT.zcache by content) as FILE.zst, which
// the library fetches and decodes itself (app.js, pack.json). No service
// worker: the host sends the isolation headers (serve.mjs, _headers). --bench keeps the recorded benchmark settings
// (scripted input, the 4x clock); without it they are dropped, so a player
// gets the real clock and their own input. --drop removes a guest path from
// the bundle (e.g. a benchmark's seed-pinning mod).

import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { copyFileSync, cpSync, existsSync, lstatSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const av = process.argv.slice(2);
const flag = (n) => av.includes(n);
const opt = (n, d) => { const i = av.indexOf(n); return i >= 0 ? av[i + 1] : d; };
const opts = (n) => av.flatMap((a, i) => (a === n ? [av[i + 1]] : []));
const [project, out] = av.filter((a, i) => !a.startsWith('--') && !['--title', '--drop', '--overlay', '--level', '--input', '--args', '--wgsl-cache'].includes(av[i - 1]));
// A W3 page's site (below): --strip takes the debug sections out of every
// .wasm (the bundle's included), --prune leaves out files the browser never
// reads (precompiled .cwasm, static archives), --dedupe puts identical files
// next to each other in the tar so zstd's window stores the second as a match.
if (!project || !out) {
  console.error('usage: node web/eject-site.mjs PROJECT OUT [--title T] [--bench] [--drop GUEST_PATH]... [--overlay DIR]... [--input SCRIPT] [--args "A B"] [--wgsl-cache FILE] [--level N]');
  process.exit(2);
}
const level = opt('--level', '19');
// A W3 page's project (gexc eject of a tools/eject/record-w3.mjs recording):
// build/web/w3.json instead of nt.json. See `w3Site` below.
const w3 = existsSync(join(project, 'build/web/w3.json')) ? JSON.parse(readFileSync(join(project, 'build/web/w3.json'), 'utf8')) : null;
const nt = w3 ? null : JSON.parse(readFileSync(join(project, 'build/web/nt.json'), 'utf8'));
const title = opt('--title', w3 ? (w3.title ?? 'gex').replace(/^Untitled - /, '') : nt.exe.split('/').pop().replace(/\.exe$/i, ''));
const zcache = `${out.replace(/\/+$/, '')}.zcache`;
rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });
mkdirSync(zcache, { recursive: true });

const files = {}; // site path -> { size, zsize }
const sha = (b) => createHash('sha256').update(b).digest('hex');

/** Put `src` at site path `path`: compressed (FILE.zst) when large. */
function place(src, path) {
  const dst = join(out, path);
  mkdirSync(dirname(dst), { recursive: true });
  const size = statSync(src).size;
  if (size < 65536) {
    copyFileSync(src, dst);
    return;
  }
  const bytes = readFileSync(src);
  const z = join(zcache, `${sha(bytes)}-${level}.zst`);
  if (!existsSync(z)) execFileSync('zstd', [`-${level}`, '--long=27', '-T4', '-q', '-f', src, '-o', z], { stdio: 'inherit' });
  copyFileSync(z, `${dst}.zst`);
  files[`/${path}`] = { size, zsize: statSync(z).size };
}

function walk(dir, fn, base = dir) {
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    if (e.isDirectory()) walk(p, fn, base);
    else if (e.isFile() || e.isSymbolicLink()) fn(p, relative(base, p));
  }
}

if (w3) {
  w3Site();
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

// Largest first: noita.exe's unit is most of the compile.
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
const bench = /^(GEX_NT_INPUT|GEX_NT_TIME_SCALE|GEX_NT_FIXED_TIME)=/;
let settings = flag('--bench') ? nt.settings : nt.settings.filter((s) => !bench.test(s));
// --input SCRIPT: GEX_NT_INPUT instead of the recorded one (a benchmark's
// clicks, by presented frame, as pewasm's NOITA_INPUT).
if (opt('--input', null)) settings = [...settings.filter((s) => !s.startsWith('GEX_NT_INPUT=')), `GEX_NT_INPUT=${opt('--input', null)}`];
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

/** The site of a W3 page's project: its page (built with the project's
 *  features, as plain files), its bundle and units (packed, decoded by the
 *  page), the launcher and the hosting files. No service worker. */
function w3Site() {
  const web = join(project, 'build/web');
  const work = `${out.replace(/\/+$/, '')}.work`;
  rmSync(work, { recursive: true, force: true });
  mkdirSync(work, { recursive: true });
  const strip = flag('--strip');
  const stripped = (src) => {
    if (!strip || !src.endsWith('.wasm')) return src;
    const dst = join(work, 'strip', sha(readFileSync(src)));
    mkdirSync(dirname(dst), { recursive: true });
    if (!existsSync(dst)) execFileSync('wasm-tools', ['strip', '-d', '^(\\.debug_.*|producers|target_features|external_debug_info|sourceMappingURL)$', src, '-o', dst]);
    return dst;
  };
  // The page as plain files: nothing between it and the server decodes, so
  // only what the page itself fetches and decodes (the bundle, the units:
  // app.js, pack.json) is packed. A host's own compression still applies.
  walk(join(web, 'page'), (p, rel) => {
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
  let lazyFiles = 0, lazyBytes = 0;
  if (strip || flag('--prune') || flag('--dedupe') || split) {
    const x = join(work, 'tarx');
    mkdirSync(x, { recursive: true });
    execFileSync('tar', ['-xf', tar, '-C', x]);
    if (strip) walk(x, (p) => { if (p.endsWith('.wasm')) copyFileSync(stripped(p), p); });
    if (flag('--prune')) walk(x, (p) => { if (/\.(cwasm|a)$/.test(p)) rmSync(p); });
    if (split) {
      split.add(w3.exe); // the page reads the loader from the bundle itself
      const store = join(out, 'project/build/web/lazy');
      mkdirSync(store, { recursive: true });
      const lazy = [];
      walk(x, (p, rel) => {
        const st = lstatSync(p);
        if (!st.isFile() || split.has(`/${rel}`)) return;
        const b = readFileSync(p);
        const h = sha(b);
        if (!existsSync(join(store, h))) writeFileSync(join(store, h), b);
        // Its time, for `mtimes=1` (wineboot compares wine.inf's with the prefix's).
        lazy.push([`/${rel}`, st.size, h, Math.trunc(st.mtimeMs / 1000)]);
        rmSync(p);
      });
      lazyFiles = lazy.length;
      lazyBytes = lazy.reduce((a, f) => a + f[1], 0);
      writeFileSync(join(out, 'project/build/web/lazy.json'), JSON.stringify({ base: '/project/build/web/lazy/', files: lazy }));
      w3.params = { ...(w3.params ?? {}), lazy: '/project/build/web/lazy.json' };
    }
    tar = join(work, 'rootfs.tar');
    const dirs = [];
    const byHash = new Map();
    const all = (d, rel) => {
      for (const e of readdirSync(d, { withFileTypes: true })) {
        const r = rel ? `${rel}/${e.name}` : e.name;
        const p = join(d, e.name);
        if (e.isDirectory()) { dirs.push(r); all(p, r); continue; }
        const k = flag('--dedupe') && e.isFile() ? sha(readFileSync(p)) : `path:${r}`;
        if (!byHash.has(k)) byHash.set(k, []);
        byHash.get(k).push(r);
      }
    };
    all(x, '');
    const list = join(work, 'tar-list');
    writeFileSync(list, [...dirs, ...[...byHash.values()].flat()].join('\n') + '\n');
    execFileSync('tar', ['-C', x, '--no-recursion', '-cf', tar, '-T', list]);
  }
  // What the split leaves in the bundle (the run's read set) is all the
  // download has of it; nt.json carries the rest's manifest by URL.
  if (w3.bundle) w3.bundle = { split: !!split, ...(split ? { read: split.size, lazy: lazyFiles, lazy_bytes: lazyBytes } : {}) };
  place(tar, 'project/build/web/rootfs.tar');
  // The units, largest first in the preload list.
  const preload = [];
  for (const u of w3.units) {
    const d = join(web, 'units', u.key);
    place(join(d, 'c.wasm'), `project/build/web/units/${u.key}/c.wasm`);
    place(join(d, 'c.entries'), `project/build/web/units/${u.key}/c.entries`);
    preload.push([`cgen:${u.key}`, `/project/build/web/units/${u.key}/c.wasm`, statSync(join(d, 'c.wasm')).size]);
    preload.push([`file:${u.key}/c.entries`, `/project/build/web/units/${u.key}/c.entries`, 0]);
  }
  preload.sort((a, b) => b[2] - a[2]);
  writeFileSync(join(out, 'project/build/web/units/preload.json'), JSON.stringify(preload.map(([a, b]) => [a, b])));
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
  walk(out, (p, rel) => { if (!p.endsWith('.zst') && !rel.startsWith('project/build/web/lazy/')) small += statSync(p).size; });
  writeFileSync(join(out, 'site.json'), JSON.stringify({ build, kind: 'w3', features_off: w3.features_off, units: w3.units.length, download: ztotal + small, unpacked: total + small, lazy: split ? { files: lazyFiles, bytes: lazyBytes } : null }, null, 1));
  console.log(`${out}: W3 build ${build}; features off: ${w3.features_off.join(', ') || 'none'}; ${w3.units.length} units; download ${mb(ztotal + small)} MB, unpacked ${mb(total + small)} MB`
    + (split ? `; bundle split: ${split.size} files read by the recorded run in it, ${lazyFiles} lazy (${mb(lazyBytes)} MB, read on demand)` : ''));
}
console.log(`${out}: build ${build}; ${stale} files of superseded units left out; ${Object.keys(files).length} packed files, ${mb(total)} MB -> ${mb(ztotal)} MB, plus ${mb(small)} MB unpacked small files; units ${mb(unitBytes)} MB raw`);
