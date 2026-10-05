#!/usr/bin/env node
// An ejected Windows program's web build as a static site, in one command:
//
//   node web/eject-site.mjs PROJECT OUT [--title T] [--bench] [--drop GUEST_PATH]... [--level N]
//
// PROJECT is a built project (`gexc eject ... --web-shim ... --web-page ...`,
// then ./build.sh). OUT gets:
//   index.html     the launcher (web/eject-site/index.html): installs sw.js,
//                  downloads everything with a progress bar, opens nt.html
//   sw.js          cross-origin isolation for hosts that can't send the
//                  headers, and FILE.zst decoded and cached as FILE
//   nt.html, assets/, gex/   the page (runtime/web/page), runtime and Mesa
//   project/build/web/nt.json, project/build/web/rootfs.tar.zst, and the
//                  units the browser loads (project/build/units/...)
//   pack.json      every packed file: size, compressed size; the build id
//   _headers       caching and isolation headers (Netlify, Cloudflare Pages)
//   serve.mjs      a static server with the same headers (node serve.mjs [port])
//   README.txt     hosting notes
// Every file over 64 KiB is zstd-compressed (-19 --long=27, cached beside OUT
// in OUT.zcache by content). --bench keeps the recorded benchmark settings
// (scripted input, the 4x clock); without it they are dropped, so a player
// gets the real clock and their own input. --drop removes a guest path from
// the bundle (e.g. a benchmark's seed-pinning mod).

import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { copyFileSync, cpSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const av = process.argv.slice(2);
const flag = (n) => av.includes(n);
const opt = (n, d) => { const i = av.indexOf(n); return i >= 0 ? av[i + 1] : d; };
const opts = (n) => av.flatMap((a, i) => (a === n ? [av[i + 1]] : []));
const [project, out] = av.filter((a, i) => !a.startsWith('--') && !['--title', '--drop', '--level'].includes(av[i - 1]));
if (!project || !out) {
  console.error('usage: node web/eject-site.mjs PROJECT OUT [--title T] [--bench] [--drop GUEST_PATH]... [--level N]');
  process.exit(2);
}
const level = opt('--level', '19');
const nt = JSON.parse(readFileSync(join(project, 'build/web/nt.json'), 'utf8'));
const title = opt('--title', nt.exe.split('/').pop().replace(/\.exe$/i, ''));
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

// The page and the runtime.
walk(join(project, 'runtime/web/page'), (p, rel) => place(p, rel));

// The units the browser loads: each unit's module and entries, region
// entries, providers, stateful providers, indirect-call profiles (part of a
// unit's key) and the link groups' index. Not the native build's (.cwasm,
// objects, recovery, precompiled).
const cgen = nt.cgen;
const units = join(project, cgen);
let unitBytes = 0;
walk(units, (p, rel) => {
  const top = rel.split('/')[0];
  if (['objects', 'recovery', 'precompiled'].includes(top)) return;
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
});

// The bundle, without what --drop names.
const drops = opts('--drop');
let tar = join(project, 'build/web/rootfs.tar');
if (drops.length) {
  tar = join(zcache, 'rootfs.tar');
  execFileSync('tar', ['-C', join(project, 'build/rootfs'), '-chf', tar, ...drops.map((d) => `--exclude=.${d}`), '.']);
}
place(tar, 'project/build/web/rootfs.tar');

// How to start: the recorded settings, less the benchmark's.
const bench = /^(GEX_NT_INPUT|GEX_NT_TIME_SCALE|GEX_NT_FIXED_TIME)=/;
const ntOut = { ...nt, settings: flag('--bench') ? nt.settings : nt.settings.filter((s) => !bench.test(s)) };
mkdirSync(join(out, 'project/build/web'), { recursive: true });
writeFileSync(join(out, 'project/build/web/nt.json'), JSON.stringify(ntOut, null, 2));

// The launcher, the service worker and its decoder.
// The build: everything served, packed or not (a page-only change is a new build too).
const smallHash = createHash('sha256');
walk(out, (p, rel) => { smallHash.update(rel); smallHash.update(readFileSync(p)); });
const build = sha(JSON.stringify(files) + JSON.stringify(ntOut) + smallHash.digest('hex')).slice(0, 12);
writeFileSync(join(out, 'index.html'), readFileSync(join(here, 'eject-site/index.html'), 'utf8').replaceAll('@TITLE@', title));
copyFileSync(join(here, 'eject-site/sw.js'), join(out, 'sw.js'));
cpSync(join(here, 'eject-site/vendor'), join(out, 'vendor'), { recursive: true });
writeFileSync(join(out, 'pack.json'), JSON.stringify({ build, files }, null, 1));

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
  const immutable = path.endsWith('.zst') || path.startsWith('/assets/');
  res.writeHead(200, {
    'Content-Type': types[extname(file)] ?? 'application/octet-stream', 'Content-Length': st.size,
    'Cross-Origin-Opener-Policy': 'same-origin', 'Cross-Origin-Embedder-Policy': 'require-corp', 'Cross-Origin-Resource-Policy': 'same-origin',
    'Cache-Control': immutable ? 'public, max-age=31536000, immutable' : 'no-cache',
  });
  if (req.method === 'HEAD') { res.end(); return; }
  createReadStream(file).pipe(res);
}).listen(port, () => console.log(\`http://localhost:\${port}/\`));
`);

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
A host that cannot send the isolation headers still works over HTTPS:
sw.js adds them, and the launcher reloads once.

The .zst files are decoded by sw.js; serve them as plain bytes, with no
Content-Encoding. nginx:
  location / {
    add_header Cross-Origin-Opener-Policy same-origin always;
    add_header Cross-Origin-Embedder-Policy require-corp always;
    types { application/javascript mjs js; application/wasm wasm; application/json json; text/html html; }
  }
`);
function mb(n) { return (n / 1048576).toFixed(1); }
console.log(`${out}: build ${build}; ${Object.keys(files).length} packed files, ${mb(total)} MB -> ${mb(ztotal)} MB, plus ${mb(small)} MB unpacked small files; units ${mb(unitBytes)} MB raw`);
