#!/usr/bin/env node
// A 32-bit Wine program as a static site on gex's own page (nt.html), with no eject:
//
//   node site/page-site.mjs OUT --tar BUNDLE.tar --units DIR --params FILE
//        [--strip] [--prune] [--dedupe] [--split] [--boot-units FILE] [--level N] [--param K=V]...
//
// - The page: page/nt.html and the app library, built with vite into OUT as
//   gex builds its page (every feature in: translation, the function JIT);
//   the x86 runtime (shim, kernel, seed) under gex/; the zstd decoder under
//   vendor/.
// - The bundle, BUNDLE.tar (a 32-bit Wine prefix and dist, as a recording's), at
//   /bundle.tar. --strip takes the debug sections out of its .wasm, --prune
//   leaves out what the browser never reads (.cwasm, static archives),
//   --dedupe orders it by content so zstd's window stores a second copy as a
//   match, --split ships the files the recording read in it and the rest as
//   lazy files (site/site-bundle.mjs, as site/eject-site.mjs does to an ejected
//   one).
// - The units, DIR (a unit cache: `gexc units build ... --preload /units`),
//   at /units, with its preload.json: the page fetches and compiles them while
//   it loads, and no unit is a synchronous fetch. --boot-units FILE (guest
//   paths, one a line: the images whose units the program loads before it is
//   up, a shell's taskbar) keeps those units in the boot's download and puts
//   the recording's other units in the background fill, /fill.bin, which the
//   page downloads while the guest runs and compiles a unit of when a process
//   first maps it (site/site-bundle.mjs `buildFill`, as site/eject-site.mjs's).
// - index.html: nt.html with the program's query, FILE's (a JSON object of
//   nt.html parameters, or a recording, wine32-session.json, whose page's query
//   it takes), less what the site decides (tar, cgen, cgenurl, preload,
//   pack), plus each --param.
// - pack.json: every file over 64 KiB (the units' over 4 KiB) is FILE.zst
//   (-19 --long=27), which the page fetches and decodes itself (app.js); a
//   cache of the compressed files sits beside OUT in OUT.zcache.
// No service worker: the host sends the isolation headers (_headers,
// serve.mjs, as site/eject-site.mjs writes them).
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { buildFill, buildRest, extract, hasRestOnly, pack, pruneTree, readLessRestOnly, readPathList, restOnly, splitTree, stripper, stripTree, unitGuest, walk } from './site-bundle.mjs';

const web = join(dirname(fileURLToPath(import.meta.url)), '..', 'page');
// A wine32 recording's format (site/record-wine32.mjs), and the name ones made before the rename carry.
const WINE32_FORMATS = ['gex-wine32-session', 'gex-w3-session'];
const av = process.argv.slice(2);
const flag = (n) => av.includes(n);
const opt = (n, d) => { const i = av.indexOf(n); return i >= 0 ? av[i + 1] : d; };
const opts = (n) => av.flatMap((a, i) => (a === n ? [av[i + 1]] : []));
const valued = ['--tar', '--units', '--params', '--level', '--param', '--boot-units'];
const [outArg] = av.filter((a, i) => !a.startsWith('--') && !valued.includes(av[i - 1]));
const out = outArg && resolve(outArg);
const tarIn = opt('--tar'), unitsIn = opt('--units'), paramsIn = opt('--params');
if (!out || !tarIn || !unitsIn || !paramsIn) {
  console.error('usage: node site/page-site.mjs OUT --tar BUNDLE.tar --units DIR --params FILE [--strip] [--prune] [--dedupe] [--split] [--boot-units FILE] [--level N] [--param K=V]...');
  process.exit(2);
}
const level = opt('--level', '19');
const zcache = `${out.replace(/\/+$/, '')}.zcache`;
const work = `${out.replace(/\/+$/, '')}.work`;
rmSync(out, { recursive: true, force: true });
rmSync(work, { recursive: true, force: true });
mkdirSync(out, { recursive: true });
mkdirSync(zcache, { recursive: true });
mkdirSync(work, { recursive: true });
const sha = (b) => createHash('sha256').update(b).digest('hex');
const files = {};

/** Put `src` at site path `path`: FILE.zst (in pack.json) when large. */
function place(src, path) {
  const dst = join(out, path);
  mkdirSync(dirname(dst), { recursive: true });
  const size = statSync(src).size;
  if (size < (/^units\//.test(path) ? 4096 : 65536)) {
    copyFileSync(src, dst);
    return;
  }
  const bytes = readFileSync(src);
  const h = sha(bytes);
  const z = join(zcache, `${h}.${level}.zst`);
  if (!existsSync(z)) execFileSync('zstd', ['-q', `-${level}`, ...(Number(level) > 19 ? ['--ultra'] : []), '--long=27', '-T0', src, '-o', z]);
  copyFileSync(z, `${dst}.zst`);
  files[`/${path}`] = { size, zsize: statSync(z).size, h: h.slice(0, 16) };
}


const strip = stripper(work, true);

// The page: nt.html and the library, as gex's own build makes them.
const config = join(web, `.page-site-${process.pid}.mjs`);
writeFileSync(config, `import base from '${web}/vite.config.js';
export default { ...base, root: '${web}', publicDir: false, logLevel: 'error',
  build: { ...base.build, outDir: '${join(work, 'page')}', emptyOutDir: true, rollupOptions: { input: { nt: '${web}/nt.html' } } } };
`);
try {
  execFileSync(join(web, 'node_modules/.bin/vite'), ['build', '--config', config], { cwd: web, stdio: 'inherit' });
} finally {
  rmSync(config, { force: true });
}
walk(join(work, 'page'), (p, rel) => { mkdirSync(dirname(join(out, rel)), { recursive: true }); copyFileSync(p, join(out, rel)); });
for (const f of ['gex_browser.x86.shared.wasm', 'gex_kernel.x86.wasm', 'seed.x86.shared.wasm']) place(strip(join(web, 'public/gex', f)), `gex/${f}`);
mkdirSync(join(out, 'vendor'), { recursive: true });
for (const f of ['zstddec-stream.mjs', 'zstddec.wasm', 'zstddec-LICENSE']) copyFileSync(join(web, 'vendor/zstddec', f), join(out, 'vendor', f));
const recording = JSON.parse(readFileSync(paramsIn, 'utf8'));
// A page with OpenGL (`gl=1`, the parameters' or the recording's query): the
// page's Mesa, which the page fetches itself, uncompressed.
const glParam = recording.url ? new URL(recording.url).searchParams.get('gl') : recording.gl;
if (String(glParam) === '1' || opts('--param').includes('gl=1')) {
  const mesa = ['gex-wgpu-replay.js', 'gex-wgpu-replay.wasm', 'gex-wgpu-replay.deferred.wasm'].filter((f) => existsSync(join(web, 'public/gex', f)));
  if (mesa.length < 2) {
    console.error('page-site: gl=1 needs the page\'s Mesa, page/public/gex/gex-wgpu-replay.{js,wasm} (ports/mesa/build.sh)');
    process.exit(1);
  }
  for (const f of mesa) copyFileSync(join(web, 'public/gex', f), join(out, 'gex', f));
}

// The bundle: stripped, pruned, deduplicated, and split as an ejected site's
// (site/site-bundle.mjs): with --split, the files the recorded run opened stay
// in it and every other file is a lazy file under /lazy/, read from the host
// when a guest first opens it (app.js `lazy`).
let tar = tarIn;
let lazyNote = '';
// A dist's rest-only files (site-bundle.mjs `REST_ONLY`: a side module the
// loader dlopens when a program first needs it) leave the bundle whatever
// the recording read, with or without --split, for the rest bundle,
// /lazy/rest.bin: compressed, fetched by its frames on the first read.
const restOnlyIn = hasRestOnly(tar);
if (flag('--strip') || flag('--prune') || flag('--dedupe') || flag('--split') || restOnlyIn) {
  const tree = extract(tar, work);
  if (flag('--strip')) stripTree(tree.x, stripper(work, true));
  if (flag('--prune')) pruneTree(tree.x);
  let read = null;
  if (flag('--split')) {
    if (!WINE32_FORMATS.includes(recording.format) || !Array.isArray(recording.files_read)) {
      console.error(`page-site: --split needs a recording with files_read as --params (${paramsIn})`);
      process.exit(1);
    }
    read = new Set(recording.files_read);
  }
  const only = restOnlyIn ? restOnly(tree.x) : new Set();
  if (restOnlyIn) read = readLessRestOnly(tree.x, read);
  if (read) {
    // the page reads the loader from the bundle itself
    const exe = WINE32_FORMATS.includes(recording.format) ? new URL(recording.url).searchParams.get('exe') : recording.exe;
    if (exe) read.add(exe);
    const store = join(out, 'lazy');
    let lazy = splitTree(tree.x, read, store);
    const rest = lazy.filter((f) => only.has(f[0]));
    if (rest.length) {
      const loose = lazy.filter((f) => !only.has(f[0]));
      const r = buildRest(rest, store, { bin: join(store, 'rest.bin'), work, frame: 8 * 1048576, zlev: Number(level) > 19 ? `--ultra -${level}` : `-${level}` });
      const keep = new Set(loose.map((f) => f[2]));
      for (const f of rest) if (!keep.has(f[2])) rmSync(join(store, f[2]), { force: true });
      lazy = [...loose, ...r.files];
      lazyNote += `; rest bundle: ${rest.length} files, ${(r.bytes / 1048576).toFixed(1)} MiB compressed`;
    }
    writeFileSync(join(out, 'lazy.json'), JSON.stringify({ base: '/lazy/', files: lazy }));
    lazyNote = `; bundle split: ${read.size} read, ${lazy.length} lazy (${(lazy.reduce((a, f) => a + f[1], 0) / 1048576).toFixed(1)} MiB)` + lazyNote;
  }
  tar = join(work, 'bundle.tar');
  pack(tree, tar, work, flag('--dedupe'));
}
place(tar, 'bundle.tar');

// The units.
if (!existsSync(join(unitsIn, 'preload.json'))) {
  console.error(`page-site: ${unitsIn} has no preload.json (gexc units build ... --preload /units)`);
  process.exit(1);
}
// --boot-units: the units the boot does not need go to the fill, not /units.
let fillNote = '';
let inFill = new Set();
if (opt('--boot-units')) {
  if (!Array.isArray(recording.units)) {
    console.error(`page-site: --boot-units needs a recording with units as --params (${paramsIn})`);
    process.exit(1);
  }
  const boot = new Set(readPathList(opt('--boot-units')).map(unitGuest));
  const fl = buildFill({ boot, units: recording.units, unitDir: unitsIn, lazyUrl: (h) => `/lazy/${h}` });
  if (fl.files.length) {
    inFill = fl.keys;
    writeFileSync(join(work, 'fill.bin'), fl.bytes);
    place(join(work, 'fill.bin'), 'fill.bin');
    writeFileSync(join(out, 'fill.json'), JSON.stringify({ url: '/fill.bin', size: fl.bytes.length, files: fl.files, demand: fl.demand }));
    fillNote = `; the fill: ${fl.keys.size} units, ${(fl.bytes.length / 1048576).toFixed(1)} MiB`;
  }
}
const fillKey = (rel) => inFill.has(rel.split('/')[0]);
mkdirSync(join(out, 'units'), { recursive: true });
walk(unitsIn, (p, rel) => {
  if (rel === 'preload.json') {
    // A preload item is [NAME, URL]: `cgen:KEY` or `file:KEY/FILE`, URL /units/KEY/...
    const list = JSON.parse(readFileSync(p, 'utf8')).filter(([, url]) => !fillKey(String(url).replace(/^\/units\//, '')));
    writeFileSync(join(out, 'units/preload.json'), JSON.stringify(list));
  } else if (!fillKey(rel) && !rel.startsWith('recovery/')) place(p, `units/${rel}`);
  // (recovery/: the cgen build's own cache, the functions it recovered per
  // image, which the page never reads: gigabytes once units.sh built there)
});

// The launch.
const build = sha(JSON.stringify(files)).slice(0, 12);
writeFileSync(join(out, 'pack.json'), JSON.stringify({ build, files }, null, 1));
const params = WINE32_FORMATS.includes(recording.format) ? Object.fromEntries(new URL(recording.url).searchParams) : { ...recording };
for (const k of ['tar', 'cgen', 'cgenurl', 'preload', 'pack', 'lazy', 'fill']) delete params[k];
if (lazyNote) params.lazy = '/lazy.json';
if (fillNote) params.fill = '/fill.json';
Object.assign(params, { tar: '/bundle.tar', cgen: '1', cgenurl: '/units', preload: '/units/preload.json', pack: `/pack.json?v=${build}` });
for (const kv of opts('--param')) { const [k, ...v] = kv.split('='); params[k] = v.join('='); }
const query = new URLSearchParams(params).toString();
writeFileSync(join(out, 'index.html'), `<!doctype html><meta charset="utf-8"><title>gex</title>
<script>location.replace('nt.html?${query}' + (location.search ? '&' + location.search.slice(1) : ''));</script>
`);
writeFileSync(join(out, '_headers'), '/*\n  Cross-Origin-Opener-Policy: same-origin\n  Cross-Origin-Embedder-Policy: require-corp\n  Cross-Origin-Resource-Policy: same-origin\n');
rmSync(work, { recursive: true, force: true });
const z = Object.values(files).reduce((a, f) => a + f.zsize, 0);
const units = Object.entries(files).filter(([p]) => p.startsWith('/units/')).reduce((a, [, f]) => a + f.zsize, 0);
console.log(`page-site: ${out}: ${(z / 1048576).toFixed(1)} MiB packed (units ${(units / 1048576).toFixed(1)} MiB), build ${build}${lazyNote}${fillNote}`);
