// The runtime API's browser tests: a vite dev server on web/ (cross-origin
// isolated), headless Chromium driven over CDP (web/test/cdp.mjs) with every
// Worker's console relayed, and one test page per argument, each of which
// sets `globalThis.__rt = { done, ok, lines }`.
//
//   node web/runtime/test/run.mjs [PAGE...]      (default: every *.html here)
//
// Env: BUSYBOX (a wasm32 busybox; the test tree is it and its applets),
// GEXCC (gexcc, to build gex-host into web/public/gex when it is missing or
// older than its source), CHROME, RT_PORT (vite; Chromium's is RT_PORT+1),
// RT_TIMEOUT (seconds per page, default 120), RT_WORK (scratch directory),
// RT_VERBOSE=1 (every console line), RT_ARCH (the translated arch, else the
// tree's).

import { spawn, execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readdirSync, readFileSync, rmSync, statSync, linkSync, copyFileSync, mkdtempSync, writeFileSync, symlinkSync, realpathSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname, basename } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Cdp, browserWsUrl } from '../../test/cdp.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const WEB = join(HERE, '..', '..');
const ROOT = join(WEB, '..');
const PORT = Number(process.env.RT_PORT || 5411);
const CPORT = PORT + 1;
const TIMEOUT = Number(process.env.RT_TIMEOUT || 120) * 1000;
const VERBOSE = !!process.env.RT_VERBOSE;
const log = (s) => process.stderr.write(`[rt] ${s}\n`);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const WORK = process.env.RT_WORK || mkdtempSync(join(tmpdir(), 'gex-rt-'));
mkdirSync(WORK, { recursive: true });

// -- gex-host --------------------------------------------------------------
const HOSTCALL_SRC = join(WEB, 'runtime/host/gex-host.c');
const HOSTCALL = join(WEB, 'public/gex/gex-host');
if (!existsSync(HOSTCALL) || statSync(HOSTCALL).mtimeMs < statSync(HOSTCALL_SRC).mtimeMs) {
  const cc = process.env.GEXCC || 'gexcc';
  log(`building ${HOSTCALL}`);
  execFileSync(cc, ['-O2', '-Wall', '-o', HOSTCALL, HOSTCALL_SRC], { stdio: 'inherit' });
}

// -- the test tree: busybox and its applets --------------------------------
const BUSYBOX = process.env.BUSYBOX;
if (!BUSYBOX || !existsSync(BUSYBOX)) throw new Error('BUSYBOX: a wasm32 busybox');
const tree = join(WORK, 'tree');
rmSync(tree, { recursive: true, force: true });
for (const d of ['bin', 'usr/bin', 'etc', 'tmp', 'root', 'home/user', 'dev', 'proc']) mkdirSync(join(tree, d), { recursive: true });
copyFileSync(BUSYBOX, join(tree, 'bin/busybox'));
for (const name of readFileSync(join(ROOT, 'tools/wine-wasm/busybox.applets'), 'utf8').split('\n').filter(Boolean)) {
  linkSync(join(tree, 'bin/busybox'), join(tree, 'bin', name));
}
// And what busybox has no applet for (probe/rt-probe.c), a Wayland and an
// X11 window (probe/rt-wl.c, rt-x11.c), and game controllers (rt-joy.c).
execFileSync(process.env.GEXCC || 'gexcc', ['-O2', '-Wall', '-o', join(tree, 'bin/rt-probe'), join(HERE, 'probe/rt-probe.c'), '-lm'], { stdio: 'inherit' });
execFileSync(process.env.GEXCC || 'gexcc', ['-O2', '-Wall', '-o', join(tree, 'bin/rt-wl'), join(HERE, 'probe/rt-wl.c'), '-lwayland-client', '-lffi'], { stdio: 'inherit' });
execFileSync(process.env.GEXCC || 'gexcc', ['-O2', '-Wall', '-o', join(tree, 'bin/rt-joy'), join(HERE, 'probe/rt-joy.c')], { stdio: 'inherit' });
execFileSync(process.env.GEXCC || 'gexcc', ['-O2', '-Wall', '-o', join(tree, 'bin/rt-x11'), join(HERE, 'probe/rt-x11.c'), '-lX11', '-lX11-xcb', '-lxcb-xfixes', '-lxcb', '-lXau', '-lXdmcp'], { stdio: 'inherit' });
// An extended attribute, which the tar carries (tar --xattrs).
execFileSync('sh', ['-c', 'echo xattr > "$1/etc/xattr" && setfattr -n user.rt -v kept "$1/etc/xattr"', 'sh', tree]);
execFileSync('tar', ['--xattrs', '-cf', join(WORK, 'bb.tar'), '-C', tree, '.']);
execFileSync('zstd', ['-q', '-f', '-3', join(WORK, 'bb.tar'), '-o', join(WORK, 'bb.tar.zst')]);
// A Debian image and its apt repository (slim.html): RT_SLIM names the
// directory (debian-slim.tar.zst, repo/), served at /__rt/slim/, with
// repo.json listing the repository for the page's provider.
rmSync(join(WORK, 'slim'), { force: true });
if (process.env.RT_SLIM) {
  symlinkSync(realpathSync(process.env.RT_SLIM), join(WORK, 'slim'));
  const list = [];
  const walk = (d, rel) => {
    for (const e of readdirSync(d, { withFileTypes: true })) {
      const p = `${rel}/${e.name}`;
      if (e.isDirectory()) { list.push([p, 0, 'directory']); walk(join(d, e.name), p); }
      else if (e.isFile()) list.push([p, statSync(join(d, e.name)).size, 'file']);
    }
  };
  walk(join(process.env.RT_SLIM, 'repo'), '');
  writeFileSync(join(WORK, 'slim-repo.json'), JSON.stringify(list));
} else {
  writeFileSync(join(WORK, 'slim-repo.json'), 'null');
}
// A lazy file's bytes (persist.html: never fetched for a snapshot).
writeFileSync(join(WORK, 'lazy.bin'), Buffer.alloc(3 << 20, 7));

// -- the package -----------------------------------------------------------
// For package.html: web/runtime/build.sh into the work directory.
const PAGES = process.argv.slice(2).length ? process.argv.slice(2) : readdirSync(HERE).filter((f) => f.endsWith('.html'));
if (PAGES.some((p) => basename(p) === 'package.html')) {
  log('building the package');
  execFileSync(join(WEB, 'runtime/build.sh'), [join(WORK, 'pkg')], { stdio: ['ignore', 'ignore', 'inherit'] });
}

// -- vite --------------------------------------------------------------------
const { createServer } = await import(join(WEB, 'node_modules/vite/dist/node/index.js'));
const base = (await import(join(WEB, 'vite.config.js'))).default;
const server = await createServer({
  ...base,
  configFile: false,
  root: WEB,
  logLevel: 'warn',
  server: { ...base.server, port: PORT, strictPort: true, host: '127.0.0.1' },
  plugins: [{
    name: 'rt-work',
    configureServer(s) {
      // The scratch files, at /__rt/NAME, with ranges (lazy files).
      // How many times each /__rt/ file was asked for: /__rt/__hits?NAME.
      const hits = new Map();
      s.middlewares.use('/__rt/', (req, res, next) => {
        const name = decodeURIComponent(req.url.split('?')[0]).replace(/^\/+/, '');
        if (name === '__hits') {
          res.setHeader('Content-Type', 'application/json');
          return res.end(JSON.stringify(hits.get(decodeURIComponent(req.url.split('?')[1] ?? '')) ?? 0));
        }
        hits.set(name, (hits.get(name) ?? 0) + 1);
        const f = join(WORK, decodeURIComponent(req.url.split('?')[0]).replace(/^\/+/, ''));
        if (!f.startsWith(WORK) || !existsSync(f) || !statSync(f).isFile()) return next();
        const all = readFileSync(f);
        res.setHeader('Cross-Origin-Resource-Policy', 'same-origin');
        // A Worker started from here is isolated as the page is.
        res.setHeader('Cross-Origin-Embedder-Policy', 'require-corp');
        res.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
        const type = { '.mjs': 'text/javascript', '.js': 'text/javascript', '.wasm': 'application/wasm', '.json': 'application/json' }[f.slice(f.lastIndexOf('.'))];
        if (type) res.setHeader('Content-Type', type);
        res.setHeader('Accept-Ranges', 'bytes');
        const m = /bytes=(\d+)-(\d*)/.exec(req.headers.range ?? '');
        if (m) {
          const a = Number(m[1]), b = m[2] ? Number(m[2]) + 1 : all.length;
          res.statusCode = 206;
          res.setHeader('Content-Range', `bytes ${a}-${b - 1}/${all.length}`);
          return res.end(all.subarray(a, b));
        }
        res.end(all);
      });
    },
  }],
});
await server.listen();
log(`vite on ${PORT}, work ${WORK}`);

// -- chromium ----------------------------------------------------------------
const CHROME = process.env.CHROME || '/usr/bin/chromium';
const profile = join(WORK, 'profile');
const chrome = spawn(CHROME, [
  '--headless=new', `--remote-debugging-port=${CPORT}`, `--user-data-dir=${profile}`, '--autoplay-policy=no-user-gesture-required',
  '--no-first-run', '--no-default-browser-check', '--enable-unsafe-webgpu', '--enable-features=Vulkan',
  '--use-angle=swiftshader', '--disable-gpu-sandbox', '--no-sandbox', 'about:blank',
], { stdio: ['ignore', 'ignore', VERBOSE ? 'inherit' : 'ignore'] });
let ws = null;
for (let i = 0; i < 100 && !ws; i++) {
  ws = await browserWsUrl(CPORT);
  if (!ws) await sleep(100);
}
if (!ws) throw new Error('chromium did not come up');
const cdp = await Cdp.connect(ws);

let failed = 0;
try {
  for (const page of PAGES) {
    const ok = await runPage(basename(page));
    if (!ok) failed++;
  }
} finally {
  cdp.close();
  chrome.kill();
  await server.close();
  if (!process.env.RT_WORK) rmSync(WORK, { recursive: true, force: true });
}
log(failed ? `${failed} page(s) failed` : 'all passed');
process.exit(failed ? 1 : 0);

async function runPage(page) {
  const { targetId } = await cdp.send('Target.createTarget', { url: 'about:blank' });
  const { sessionId } = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
  const names = new Map([[sessionId, 'page']]);
  const off = [];
  const consoleLine = (params, sid) => {
    const text = (params.args ?? []).map((a) => a.value ?? a.description ?? '').join(' ');
    if (VERBOSE || /\b(FAIL|Error|error|trap|panic)\b/.test(text)) log(`${names.get(sid) ?? sid}: ${text}`);
  };
  off.push(cdp.on('Runtime.consoleAPICalled', consoleLine));
  off.push(cdp.on('Runtime.exceptionThrown', (p, sid) => log(`${names.get(sid) ?? sid}: EXCEPTION ${p.exceptionDetails?.exception?.description ?? p.exceptionDetails?.text}`)));
  off.push(cdp.on('Target.attachedToTarget', async (p) => {
    names.set(p.sessionId, `${p.targetInfo.type}:${p.targetInfo.title || p.targetInfo.url.split('/').pop()}`);
    await cdp.send('Runtime.enable', {}, p.sessionId).catch(() => {});
    await cdp.send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: false, flatten: true }, p.sessionId).catch(() => {});
    await cdp.send('Runtime.runIfWaitingForDebugger', {}, p.sessionId).catch(() => {});
  }));
  await cdp.send('Runtime.enable', {}, sessionId);
  await cdp.send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: false, flatten: true }, sessionId);
  const url = `http://127.0.0.1:${PORT}/runtime/test/${page}${process.env.RT_ARCH ? `?arch=${process.env.RT_ARCH}` : ''}`;
  log(`${page}: ${url}`);
  const t0 = Date.now();
  await cdp.send('Page.navigate', { url }, sessionId);
  let shown = 0;
  let sent = 0;
  let result = null;
  while (Date.now() - t0 < TIMEOUT) {
    await sleep(250);
    const r = await cdp.send('Runtime.evaluate', { expression: 'JSON.stringify(globalThis.__rt ?? null)', returnByValue: true }, sessionId).catch(() => null);
    const v = r?.result?.value ? JSON.parse(r.result.value) : null;
    if (!v) continue;
    for (; shown < v.lines.length; shown++) process.stdout.write(`${page}: ${v.lines[shown]}\n`);
    // Real input the page asks for (lib.js `input`): what a user gesture
    // needs, as pointer lock does.
    if ((v.input?.length ?? 0) < sent) sent = v.input?.length ?? 0; // reloaded
    if ((v.input?.length ?? 0) > sent) {
      for (; sent < v.input.length; sent++) {
        const { method, ...params } = v.input[sent];
        await cdp.send(method, params, sessionId).catch((e) => log(`${page}: ${method}: ${e?.message ?? e}`));
      }
      await cdp.send('Runtime.evaluate', { expression: `globalThis.__rt.inputDone = ${sent}` }, sessionId).catch(() => {});
    }
    if (v.done) { result = v; break; }
  }
  for (const f of off) f();
  await cdp.send('Target.closeTarget', { targetId }).catch(() => {});
  const secs = ((Date.now() - t0) / 1000).toFixed(1);
  if (!result) {
    log(`${page}: FAIL timed out after ${secs} s`);
    return false;
  }
  log(`${page}: ${result.ok ? 'ok' : 'FAIL'} (${secs} s)`);
  return result.ok;
}
