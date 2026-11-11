// gex's test runner: test/README.md.
//
//   node test/run.mjs [--group G,...] [--host browser|node|both] [--arch a64,x86]
//                     [--jobs N] [--allow-skip] [--verbose] [--work DIR] [--artefacts DIR]
//                     [--port N] [--gpu] [--timed] [--prebuilt] [--prepare] [TEST...]
//
// One JSON line per test, host and arch on stdout; what a person reads on
// stderr. Exit 0 only when every test is ok (and skips, with --allow-skip).

import { spawn, fork } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { parseArgs } from 'node:util';
import { Cdp, browserWsUrl } from '../test/lib/cdp.mjs';
import { images, outputs } from './fixtures/images.mjs';
import { packaged } from './lib/package.mjs';
import { refuseStale } from '../page/build-inputs.mjs';

const ROOT = join(import.meta.dirname, '..');
const TESTS = import.meta.dirname;
const WEB = join(ROOT, 'page');
const ARCHES = { a64: 'aarch64', x86: 'x86' };
const HOSTS = ['browser', 'node'];

const { values: o, positionals } = parseArgs({
  allowPositionals: true,
  options: {
    group: { type: 'string' },
    host: { type: 'string', default: 'both' },
    arch: { type: 'string', default: 'a64' },
    jobs: { type: 'string', default: '4' },
    'allow-skip': { type: 'boolean', default: false },
    verbose: { type: 'boolean', default: false },
    work: { type: 'string', default: process.env.GEX_TEST_WORK },
    port: { type: 'string', default: process.env.GEX_TEST_PORT || '5411' },
    artefacts: { type: 'string', default: process.env.GEX_TEST_ARTEFACTS },
    list: { type: 'boolean', default: false },
    gpu: { type: 'boolean', default: false },
    timed: { type: 'boolean', default: false },
    prebuilt: { type: 'boolean', default: false },
    prepare: { type: 'boolean', default: false },
    package: { type: 'string', default: process.env.GEX_TEST_PACKAGE },
    single: { type: 'boolean', default: false },
  },
});
const log = (s) => process.stderr.write(`[test] ${s}\n`);
// `--package DIR`: the runtime as page/runtime/build.sh built it into DIR, not
// from source. The tests' imports of the runtime's entry modules go to the
// built ones, which find their files themselves.
const PACKAGE = o.package ? resolve(o.package) : null;
if (PACKAGE && !existsSync(join(PACKAGE, 'index.mjs'))) throw new Error(`--package: no ${PACKAGE}/index.mjs (page/runtime/build.sh DIR)`);
// `--single`: the single-thread build. Its pages are served without
// cross-origin isolation, so they have no SharedArrayBuffer: the runtime from
// source runs that build, and so must a package (--package, as
// `page/runtime/build.sh --single` builds @gexemu/runtime-single). Browser only.
// A test the single-thread build cannot run yet says why in its meta
// (`single: 'why'`) and is skipped; one that needs longer there says so
// (`singleTimeout`, seconds, in place of `timeout`).
if (o.single && o.host === 'node') throw new Error('--single: the browser host only');
if (o.single && o.host === 'both') o.host = 'browser';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** The /proc status (VmData, VmRSS, VmHWM; KiB) of the one Chromium
 *  renderer that holds more than 256 MiB (a page running a test; spare
 *  renderers hold a few dozen): null when there are several (other jobs'
 *  pages) or none. */
async function rendererStatus() {
  const info = await cdp.send('SystemInfo.getProcessInfo', {}).catch(() => null);
  const big = [];
  for (const p of info?.processInfo ?? []) {
    if (p.type !== 'renderer') continue;
    try {
      const st = readFileSync(`/proc/${p.id}/status`, 'utf8');
      const kb = (k) => Number(new RegExp(`${k}:\\s+(\\d+) kB`).exec(st)?.[1] ?? 0);
      if (kb('VmRSS') > 256 * 1024) big.push({ data: kb('VmData'), rss: kb('VmRSS'), hwm: kb('VmHWM') });
    } catch {}
  }
  return big.length === 1 ? big[0] : null;
}
const hosts = o.host === 'both' ? HOSTS : [o.host];
if (!hosts.every((h) => HOSTS.includes(h))) throw new Error(`--host: browser, node or both, not ${o.host}`);
// Timing only on a bench host, through bench.sh (which sets GBENCH_RUN).
if (o.timed && !process.env.GBENCH_RUN) throw new Error('--timed: timed runs happen only under bench.sh');
const gpu = o.gpu || o.timed;
const arches = o.arch.split(',');
for (const a of arches) if (!ARCHES[a]) throw new Error(`--arch: a64 or x86, not ${a}`);

// -- the tests ---------------------------------------------------------------
/** A test's `meta`, read from its source without running it (an object
 *  literal: `export const meta = { ... };`). */
function metaOf(file) {
  const m = /^export const meta = (\{[\s\S]*?\n?\});$/m.exec(readFileSync(file, 'utf8'));
  if (!m) throw new Error(`${file}: no \`export const meta = { ... };\``);
  return { hosts: ['browser'], arches: Object.keys(ARCHES), images: [], timeout: 120, timed: false, ...new Function(`return (${m[1]});`)() };
}
const all = [];
for (const group of readdirSync(TESTS).sort()) {
  const d = join(TESTS, group);
  if (['lib', 'fixtures'].includes(group) || !statSync(d).isDirectory()) continue;
  for (const f of readdirSync(d).sort()) {
    if (f.endsWith('.test.js')) all.push({ name: `${group}/${f.slice(0, -8)}`, group, file: join(d, f), meta: metaOf(join(d, f)) });
  }
}
const groups = o.group?.split(',');
const tests = all.filter((x) => (!groups || groups.includes(x.group)) && x.meta.timed === o.timed
  && (!positionals.length || positionals.some((p) => p === x.name || p === x.name.split('/')[1])));
if (positionals.length && !tests.length) throw new Error(`no test ${positionals.join(' ')}`);
const jobs = [];
for (const test of tests) for (const host of hosts) for (const arch of arches) if (test.meta.hosts.includes(host) && test.meta.arches.includes(arch)) jobs.push({ test, host, arch });
if (o.list) {
  for (const j of jobs) console.log(`${j.test.name} ${j.host} ${j.arch}`);
  process.exit(0);
}

// -- the images --------------------------------------------------------------
const WORK = o.work || mkdtempSync(join(tmpdir(), 'gex-test-'));
mkdirSync(WORK, { recursive: true });
// Screenshots (`t.screenshot`, and one of every browser test that fails).
const ARTEFACTS = o.artefacts || join(WORK, 'artefacts');
const missing = new Map();
for (const name of new Set(jobs.flatMap((j) => j.test.meta.images))) {
  if (!images[name]) throw new Error(`no image ${name} (test/fixtures/images.mjs)`);
  const why = o.prebuilt
    ? (outputs[name] ?? []).every((f) => existsSync(join(WORK, f))) ? null : 'not in the work directory (--prebuilt)'
    : await images[name](WORK, { single: o.single }).catch((e) => String(e?.message ?? e));
  if (why) {
    missing.set(name, why);
    log(`image ${name}: ${why}`);
  }
}
// `--prepare`: the images the tests need, made in the work directory and no
// test run (another host then runs them with --prebuilt).
if (o.prepare) process.exit(missing.size ? 1 : 0);

// -- the build ---------------------------------------------------------------
// Both hosts run page/public's kernel and shim (the page's JavaScript comes from
// source), which run the old code without a word when they are older than it.
{
  const infix = (a) => (a === 'x86' ? '.x86' : '');
  const names = [...new Set(jobs.flatMap((j) => [`gex_kernel${infix(j.arch)}`, `gex_browser${infix(j.arch)}`]))];
  if (names.length) refuseStale('test', join(resolve(WEB, process.env.GEX_PUBLIC_DIR || 'public'), 'gex'), names, 'hosts/browser/build.sh (GEX_ARCH=x86 for .x86)');
}

// -- the browser -------------------------------------------------------------
let server = null;
let chrome = null;
let cdp = null;
if (jobs.some((j) => j.host === 'browser')) ({ server, chrome, cdp } = await startBrowser());

// -- the run -----------------------------------------------------------------
let bad = 0;
// `meta.alone`: a test that reads its process's memory from /proc (the
// renderer's is told from others only when it is the one running), or one
// too big to share a renderer, goes after the rest, one at a time.
const lone = (j) => j.test.meta.alone;
for (const [queue, width] of [[jobs.filter((j) => !lone(j)), Math.max(1, Number(o.jobs))], [jobs.filter(lone), 1]]) {
  await Promise.all(Array.from({ length: width }, async () => {
    for (let j; (j = queue.shift());) {
      const r = await runJob(j);
      console.log(JSON.stringify(r));
      if (r.result === 'FAIL' || (r.result === 'skip' && !o['allow-skip'])) bad++;
      log(`${j.test.name} ${j.host} ${j.arch}: ${r.result}${r.skip ? ` (${r.skip})` : ''} (${(r.ms / 1000).toFixed(1)} s)`);
    }
  }));
}
cdp?.close();
chrome?.kill();
await server?.close();
if (!o.work) rmSync(WORK, { recursive: true, force: true });
log(bad ? `${bad} of ${jobs.length} not ok` : `all ${jobs.length} ok`);
process.exit(bad ? 1 : 0);

async function runJob({ test, host, arch }) {
  const t0 = Date.now();
  const out = { test: test.name, host, arch, result: 'FAIL', ms: 0, checks: 0, fail: [], skip: null, times: {} };
  // The package is built for the browser: its Workers are bundled for a
  // page, and under Node it would reach for its sources.
  if (PACKAGE && host === 'node') return { ...out, result: 'skip', skip: '--package runs on the browser host only' };
  if (o.single && test.meta.single) return { ...out, result: 'skip', skip: `single-thread build: ${test.meta.single}` };
  const why = test.meta.images.map((n) => missing.get(n)).find(Boolean);
  if (why) return { ...out, result: 'skip', skip: why };
  const say = (line) => {
    if (o.verbose || /^FAIL/.test(line)) process.stderr.write(`${test.name} ${host} ${arch}: ${line}\n`);
  };
  const r = await (host === 'browser' ? runPage : runNode)(test, ARCHES[arch], say, (o.single && test.meta.singleTimeout || test.meta.timeout) * 1000);
  return { ...out, ...r, ms: Date.now() - t0 };
}

// -- node ----------------------------------------------------------------------
/** The test in a Node process of its own, one per phase. */
async function runNode(test, arch, say, timeout) {
  let res = { checks: 0, fail: [], times: {} };
  const until = Date.now() + timeout;
  for (let phase = 1, state = null; ;) {
    const child = fork(join(TESTS, 'lib/node.mjs'), [test.file], { stdio: ['ignore', 'pipe', 'pipe', 'ipc'] });
    let err = '';
    // What the runtime says, for `t.logs`: its Workers write to the process's
    // own descriptors, not through anything the test could catch.
    const logs = [];
    // `t.logs`' marks (node.mjs), by stream, once read.
    const marks = new Set();
    const keep = (d, part, stream) => {
      const lines = (part + d).split('\n');
      const rest = lines.pop();
      for (const l of lines) {
        if (l.startsWith('\u0001gex-logs ')) marks.add(`${stream} ${l}`);
        else if (logs.length < 100000) logs.push(l);
      }
      return rest;
    };
    let outPart = '';
    let errPart = '';
    child.stdout.on('data', (d) => {
      outPart = keep(d, outPart, 'out');
      if (o.verbose) process.stdout.write(d);
    });
    child.stderr.on('data', (d) => {
      err = (err + d).slice(-4000);
      errPart = keep(d, errPart, 'err');
    });
    const m = await new Promise((resolve) => {
      const timer = setTimeout(() => resolve({ timeout: true }), Math.max(0, until - Date.now()));
      child.on('message', (msg) => {
        if (msg.line !== undefined) return say(msg.line);
        if (msg.logs) {
          // Once both pipes have brought the mark: all written before it.
          const { re, flags, mark } = msg.logs;
          const answer = () => child.send({ logs: logs.filter((l) => new RegExp(re, flags).test(l)) });
          if (!mark) return answer();
          const t1 = Date.now() + 10000;
          const look = () => (marks.has(`out ${mark}`) && marks.has(`err ${mark}`)) || Date.now() > t1 ? answer() : setTimeout(look, 10);
          return look();
        }
        clearTimeout(timer);
        resolve(msg);
      });
      // 'exit' can come before the last messages: those are in once the
      // channel has closed ('disconnect') too.
      let exited = null;
      let gone = false;
      const end = () => { if (exited !== null && gone) { clearTimeout(timer); resolve({ exited }); } };
      child.on('exit', (code, sig) => { exited = sig ?? code; end(); });
      child.on('disconnect', () => { gone = true; end(); });
      child.send({ arch, phase, state, res, work: WORK, gpu, timed: o.timed });
    });
    child.kill('SIGKILL');
    if (m.res) res = m.res;
    if (m.reload !== undefined) {
      phase++;
      state = m.reload;
      continue;
    }
    if (m.done) return { result: m.result, skip: m.skip, ...res };
    const what = m.timeout ? `timed out after ${timeout / 1000} s` : `exited (${m.exited}) before it ended: ${err.trim().split('\n').slice(-3).join(' / ')}`;
    say(`FAIL ${what}`);
    return { result: 'FAIL', ...res, fail: [...res.fail, what] };
  }
}

// -- browser -------------------------------------------------------------------
/** vite on web/ (cross-origin isolated; the runtime from source), serving the
 *  work directory and the test pages, and one headless Chromium. */
async function startBrowser() {
  const PORT = Number(o.port);
  const { createServer } = await import(join(WEB, 'node_modules/vite/dist/node/index.js'));
  const base = (await import(join(WEB, 'vite.config.js'))).default;
  const hits = new Map();
  /** Name parts whose next request fails (`/__t/drop`). */
  const drops = [];
  const server = await createServer({
    ...base,
    configFile: false,
    // A missing file is a 404, not the app's index.html: a page that fetches
    // an absent module then sees it absent rather than failing to compile HTML.
    appType: 'mpa',
    root: WEB,
    logLevel: 'warn',
    server: { ...base.server, ...(o.single && { headers: {} }), port: PORT, strictPort: true, host: '127.0.0.1', ...(PACKAGE && { fs: { ...base.server?.fs, allow: [...(base.server?.fs?.allow ?? []), PACKAGE] } }) },
    plugins: [...(PACKAGE ? [{
      name: 'gex-test-package',
      enforce: 'pre',
      resolveId: (source, importer) => packaged(PACKAGE, join(WEB, 'runtime/src/'), source, importer),
    }] : []), {
      name: 'gex-test',
      configureServer(s) {
        // `--package`: the built modules as they are, not through vite's
        // transforms (which would rewrite their Workers' URLs).
        if (PACKAGE) s.middlewares.use(`/@fs${PACKAGE}/`, (req, res, next) => {
          const f = join(PACKAGE, decodeURIComponent(req.url.split('?')[0]));
          if (!f.startsWith(PACKAGE) || !existsSync(f) || !statSync(f).isFile()) return next();
          if (!o.single) {
            res.setHeader('Cross-Origin-Resource-Policy', 'same-origin');
            res.setHeader('Cross-Origin-Embedder-Policy', 'require-corp');
          }
          const type = { '.mjs': 'text/javascript', '.js': 'text/javascript', '.wasm': 'application/wasm', '.json': 'application/json', '.css': 'text/css' }[f.slice(f.lastIndexOf('.'))];
          if (type) res.setHeader('Content-Type', type);
          res.end(readFileSync(f));
        });
        s.middlewares.use('/__t/', (req, res, next) => {
          const [path, query = ''] = req.url.split('?');
          const iso = () => {
            if (o.single) return;
            res.setHeader('Cross-Origin-Resource-Policy', 'same-origin');
            res.setHeader('Cross-Origin-Embedder-Policy', 'require-corp');
            res.setHeader('Cross-Origin-Opener-Policy', 'same-origin');
          };
          // A test's page: lib/browser.js runs it.
          if (path === '/page') {
            iso();
            res.setHeader('Content-Type', 'text/html');
            return res.end(`<!doctype html><meta charset="utf-8"><title>${new URLSearchParams(query).get('test')}</title><script type="module" src="/@fs${TESTS}/lib/browser.js"></script>`);
          }
          // How many times each work file was asked for.
          if (path === '/hits') {
            res.setHeader('Content-Type', 'application/json');
            return res.end(JSON.stringify(hits.get(decodeURIComponent(query)) ?? 0));
          }
          // The work files asked for whose names hold a string, summed.
          if (path === '/asked') {
            const part = decodeURIComponent(query);
            let n = 0;
            for (const [k, v] of hits) if (k.includes(part)) n += v;
            res.setHeader('Content-Type', 'application/json');
            return res.end(JSON.stringify(n));
          }
          // The next request for a work file whose name holds a string fails,
          // as a dropped connection would (503), wherever in the page's
          // realms it is made.
          if (path === '/drop') {
            drops.push(decodeURIComponent(query));
            return res.end('ok');
          }
          if (!path.startsWith('/work/')) return next();
          const name = decodeURIComponent(path.slice(6));
          hits.set(name, (hits.get(name) ?? 0) + 1);
          const drop = drops.findIndex((part) => name.includes(part));
          if (drop >= 0) {
            drops.splice(drop, 1);
            res.statusCode = 503;
            return res.end('dropped by the test');
          }
          const f = join(WORK, name);
          if (!f.startsWith(WORK) || !existsSync(f) || !statSync(f).isFile()) return next();
          const all = readFileSync(f);
          iso();
          const type = { '.mjs': 'text/javascript', '.js': 'text/javascript', '.wasm': 'application/wasm', '.json': 'application/json' }[f.slice(f.lastIndexOf('.'))];
          if (type) res.setHeader('Content-Type', type);
          res.setHeader('Accept-Ranges', 'bytes');
          const m = /bytes=(\d+)-(\d*)/.exec(req.headers.range ?? '');
          if (m) {
            const a = Number(m[1]);
            const b = m[2] ? Number(m[2]) + 1 : all.length;
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
  const profile = join(WORK, 'profile');
  rmSync(profile, { recursive: true, force: true });
  const chrome = spawn(process.env.CHROME || '/usr/bin/chromium', [
    '--headless=new', `--remote-debugging-port=${PORT + 1}`, `--user-data-dir=${profile}`, '--autoplay-policy=no-user-gesture-required',
    '--no-first-run', '--no-default-browser-check', '--enable-unsafe-webgpu', '--enable-features=Vulkan',
    // The machine's GPU (--gpu), else SwiftShader. ANGLE and WebGPU on one
    // backend either way: a canvas both read needs that.
    ...(gpu ? ['--use-angle=vulkan', '--ignore-gpu-blocklist'] : ['--use-angle=swiftshader']),
    '--disable-gpu-sandbox', '--no-sandbox', 'about:blank',
  ], { stdio: ['ignore', 'ignore', o.verbose ? 'inherit' : 'ignore'] });
  let ws = null;
  // A minute: a loaded machine starts it slowly.
  for (let i = 0; i < 600 && !ws; i++) {
    ws = await browserWsUrl(PORT + 1);
    if (!ws) await sleep(100);
  }
  if (!ws) {
    chrome.kill('SIGKILL');
    throw new Error('chromium did not come up');
  }
  return { server, chrome, cdp: await Cdp.connect(ws) };
}

/** The test in a page of its own (a fresh browser context, so its storage
 *  is its own too), every Worker's console relayed. */
async function runPage(test, arch, say, timeout) {
  // `meta.profile` (or GEX_TEST_PROFILE_CONTEXT=1): the profile's own
  // context, its storage on disk as a user's is. A fresh context's is in
  // memory, and its origin private file system has limits of its own (a
  // file stops near 2 GiB; growing one past 1 GiB by truncate ends the
  // browser).
  const own = test.meta.profile || process.env.GEX_TEST_PROFILE_CONTEXT === '1';
  const { browserContextId } = own ? {} : await cdp.send('Target.createBrowserContext', {});
  const { targetId } = await cdp.send('Target.createTarget', { url: 'about:blank', ...(browserContextId ? { browserContextId } : {}) });
  const { sessionId } = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
  const names = new Map([[sessionId, 'page']]);
  const mine = new Set([sessionId]);
  const off = [];
  // The page's and its Workers' console this phase, for `t.logs`, and each
  // line's level (`log`, `info`, `warning`, `error`, `debug`).
  const logs = [];
  const levels = [];
  off.push(cdp.on('Runtime.consoleAPICalled', (p, sid) => {
    if (!mine.has(sid)) return;
    const text = (p.args ?? []).map((a) => a.value ?? a.description ?? '').join(' ');
    if (logs.length < 100000) logs.push(text), levels.push(p.type);
    if (sid === sessionId) return; // the page's own lines come through __t
    if (o.verbose || /\b(FAIL|Error|error|trap|panic)\b/.test(text)) process.stderr.write(`${test.name}: ${names.get(sid)}: ${text}\n`);
  }));
  // A renderer that dies (out of memory, say) answers nothing again: the
  // run ends there, not at its timeout.
  let crashed = null;
  let onCrash;
  const crash = new Promise((r) => { onCrash = r; });
  const died = (what) => { crashed ??= what; onCrash(); };
  off.push(cdp.on('Inspector.targetCrashed', (p, sid) => { if (sid === sessionId) died('the page crashed'); }));
  off.push(cdp.on('Target.targetCrashed', (p) => { if (p.targetId === targetId) died(`the page crashed (${p.status}, error ${p.errorCode})`); }));
  off.push(cdp.on('Runtime.exceptionThrown', (p, sid) => {
    if (mine.has(sid)) say(`${names.get(sid)}: exception ${p.exceptionDetails?.exception?.description ?? p.exceptionDetails?.text}`);
  }));
  off.push(cdp.on('Target.attachedToTarget', async (p, sid) => {
    if (!mine.has(sid)) return;
    mine.add(p.sessionId);
    names.set(p.sessionId, `${p.targetInfo.type}:${p.targetInfo.title || p.targetInfo.url.split('/').pop()}`);
    await cdp.send('Runtime.enable', {}, p.sessionId).catch(() => {});
    await cdp.send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: false, flatten: true }, p.sessionId).catch(() => {});
    await cdp.send('Runtime.runIfWaitingForDebugger', {}, p.sessionId).catch(() => {});
  }));
  await cdp.send('Runtime.enable', {}, sessionId);
  await cdp.send('Inspector.enable', {}, sessionId).catch(() => {});
  await cdp.send('Target.setDiscoverTargets', { discover: true }).catch(() => {});
  await cdp.send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: false, flatten: true }, sessionId);
  const q = new URLSearchParams({ test: test.name, src: `/@fs${test.file}`, arch, ...(gpu ? { gpu: 1 } : {}), ...(o.timed ? { timed: 1 } : {}), ...(PACKAGE ? { pkg: 1 } : {}) });
  // GEX_TEST_QUERY: more of the page's query (a measurement's switches).
  for (const [k, v] of new URLSearchParams(process.env.GEX_TEST_QUERY ?? '')) q.set(k, v);
  const t0 = Date.now();
  await cdp.send('Page.navigate', { url: `http://127.0.0.1:${o.port}/__t/page?${q}` }, sessionId);
  let shown = 0;
  let sent = 0;
  let v = null;
  const dir = join(ARTEFACTS, `${test.name.replace('/', '-')}-${arch}`);
  const save = (name, b64) => {
    mkdirSync(dir, { recursive: true });
    const path = join(dir, name);
    writeFileSync(path, Buffer.from(b64, 'base64'));
    say(`saved ${path}`);
    return path;
  };
  const poll = 'JSON.stringify(globalThis.__t ? (({ done, lines, asks, res, result, skip }) => ({ done, lines, asks, res, result, skip }))(globalThis.__t) : null)';
  while (Date.now() - t0 < timeout && !crashed) {
    await sleep(sent ? 50 : 250);
    const r = await Promise.race([
      cdp.send('Runtime.evaluate', { expression: poll, returnByValue: true }, sessionId).catch(() => null),
      sleep(30000).then(() => null),
      crash.then(() => null),
    ]);
    const now = r?.result?.value ? JSON.parse(r.result.value) : null;
    if (!now) continue;
    v = now;
    for (; shown < v.lines.length; shown++) say(v.lines[shown]);
    // What the page asks of the browser (`t.cdp`): real input, which a user
    // gesture needs, and screenshots. Each answer goes back by its index.
    if (v.asks.length < sent) { sent = v.asks.length; logs.length = 0; } // a new phase
    for (; sent < v.asks.length; sent++) {
      const { method, save: name, ...params } = v.asks[sent];
      // Not CDP: the console lines that match (`t.logs`).
      const reply = method === '__logs' ? { result: { lines: logs.map((l, i) => (params.levels ? `${levels[i]} ${l}` : l)).filter((l) => new RegExp(params.re, params.flags).test(l)) } }
        : method === '__status' ? { result: { status: await rendererStatus() } }
        // Not CDP: a file of the page's among the artefacts (`t.saveFile`).
        : method === '__save' ? { result: { path: save(name, params.data) } } : await Promise.race([cdp.send(method, params, sessionId).then(
        (result) => ({ result: name && result?.data ? { path: save(name, result.data) } : result }),
        (e) => ({ error: String(e?.message ?? e) }),
      ), crash.then(() => ({ error: crashed }))]);
      if (crashed) break;
      await cdp.send('Runtime.evaluate', { expression: `globalThis.__t.replies[${sent}] = ${JSON.stringify(reply)}` }, sessionId).catch(() => {});
    }
    if (v.done) break;
  }
  // What the page showed when it failed.
  if (!crashed && (!v?.done || v.result === 'FAIL')) {
    const shot = await cdp.send('Page.captureScreenshot', { format: 'png' }, sessionId).catch(() => null);
    if (shot?.data) save('fail.png', shot.data);
  }
  for (const f of off) f();
  await cdp.send('Target.closeTarget', { targetId }).catch(() => {});
  if (browserContextId) await cdp.send('Target.disposeBrowserContext', { browserContextId }).catch(() => {});
  const res = v?.res ?? { checks: 0, fail: [], times: {} };
  if (!v?.done) {
    const what = crashed ?? `timed out after ${timeout / 1000} s`;
    say(`FAIL ${what}`);
    return { result: 'FAIL', ...res, fail: [...res.fail, what] };
  }
  return { result: v.result, skip: v.skip, ...res };
}
