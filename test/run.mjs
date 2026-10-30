// gex's test runner: test/README.md.
//
//   node test/run.mjs [--group G,...] [--host browser|node|both] [--arch a64,x86]
//                     [--jobs N] [--allow-skip] [--verbose] [--work DIR] [--artefacts DIR]
//                     [--port N] [TEST...]
//
// One JSON line per test, host and arch on stdout; what a person reads on
// stderr. Exit 0 only when every test is ok (and skips, with --allow-skip).

import { spawn, fork } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { parseArgs } from 'node:util';
import { Cdp, browserWsUrl } from '../web/test/cdp.mjs';
import { images } from './fixtures/images.mjs';

const ROOT = join(import.meta.dirname, '..');
const TESTS = import.meta.dirname;
const WEB = join(ROOT, 'web');
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
  },
});
const log = (s) => process.stderr.write(`[test] ${s}\n`);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const hosts = o.host === 'both' ? HOSTS : [o.host];
if (!hosts.every((h) => HOSTS.includes(h))) throw new Error(`--host: browser, node or both, not ${o.host}`);
const arches = o.arch.split(',');
for (const a of arches) if (!ARCHES[a]) throw new Error(`--arch: a64 or x86, not ${a}`);

// -- the tests ---------------------------------------------------------------
/** A test's `meta`, read from its source without running it (an object
 *  literal: `export const meta = { ... };`). */
function metaOf(file) {
  const m = /^export const meta = (\{[\s\S]*?\n?\});$/m.exec(readFileSync(file, 'utf8'));
  if (!m) throw new Error(`${file}: no \`export const meta = { ... };\``);
  return { hosts: ['browser'], arches: Object.keys(ARCHES), images: [], timeout: 120, ...new Function(`return (${m[1]});`)() };
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
const tests = all.filter((x) => (!groups || groups.includes(x.group))
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
  const why = await images[name](WORK).catch((e) => String(e?.message ?? e));
  if (why) {
    missing.set(name, why);
    log(`image ${name}: ${why}`);
  }
}

// -- the browser -------------------------------------------------------------
let server = null;
let chrome = null;
let cdp = null;
if (jobs.some((j) => j.host === 'browser')) ({ server, chrome, cdp } = await startBrowser());

// -- the run -----------------------------------------------------------------
let bad = 0;
const queue = [...jobs];
await Promise.all(Array.from({ length: Math.max(1, Number(o.jobs)) }, async () => {
  for (let j; (j = queue.shift());) {
    const r = await runJob(j);
    console.log(JSON.stringify(r));
    if (r.result === 'FAIL' || (r.result === 'skip' && !o['allow-skip'])) bad++;
    log(`${j.test.name} ${j.host} ${j.arch}: ${r.result}${r.skip ? ` (${r.skip})` : ''} (${(r.ms / 1000).toFixed(1)} s)`);
  }
}));
cdp?.close();
chrome?.kill();
await server?.close();
if (!o.work) rmSync(WORK, { recursive: true, force: true });
log(bad ? `${bad} of ${jobs.length} not ok` : `all ${jobs.length} ok`);
process.exit(bad ? 1 : 0);

async function runJob({ test, host, arch }) {
  const t0 = Date.now();
  const out = { test: test.name, host, arch, result: 'FAIL', ms: 0, checks: 0, fail: [], skip: null, times: {} };
  const why = test.meta.images.map((n) => missing.get(n)).find(Boolean);
  if (why) return { ...out, result: 'skip', skip: why };
  const say = (line) => {
    if (o.verbose || /^FAIL/.test(line)) process.stderr.write(`${test.name} ${host} ${arch}: ${line}\n`);
  };
  const r = await (host === 'browser' ? runPage : runNode)(test, ARCHES[arch], say, test.meta.timeout * 1000);
  return { ...out, ...r, ms: Date.now() - t0 };
}

// -- node ----------------------------------------------------------------------
/** The test in a Node process of its own, one per phase. */
async function runNode(test, arch, say, timeout) {
  let res = { checks: 0, fail: [], times: {} };
  const until = Date.now() + timeout;
  for (let phase = 1, state = null; ;) {
    const child = fork(join(TESTS, 'lib/node.mjs'), [test.file], { stdio: ['ignore', o.verbose ? 'inherit' : 'ignore', 'pipe', 'ipc'] });
    let err = '';
    child.stderr.on('data', (d) => { err = (err + d).slice(-4000); });
    const m = await new Promise((resolve) => {
      const timer = setTimeout(() => resolve({ timeout: true }), Math.max(0, until - Date.now()));
      child.on('message', (msg) => {
        if (msg.line !== undefined) return say(msg.line);
        clearTimeout(timer);
        resolve(msg);
      });
      child.on('exit', (code, sig) => { clearTimeout(timer); resolve({ exited: sig ?? code }); });
      child.send({ arch, phase, state, res, work: WORK });
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
  const server = await createServer({
    ...base,
    configFile: false,
    // A missing file is a 404, not the app's index.html: a page that fetches
    // an absent module then sees it absent rather than failing to compile HTML.
    appType: 'mpa',
    root: WEB,
    logLevel: 'warn',
    server: { ...base.server, port: PORT, strictPort: true, host: '127.0.0.1' },
    plugins: [{
      name: 'gex-test',
      configureServer(s) {
        s.middlewares.use('/__t/', (req, res, next) => {
          const [path, query = ''] = req.url.split('?');
          const iso = () => {
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
          if (!path.startsWith('/work/')) return next();
          const name = decodeURIComponent(path.slice(6));
          hits.set(name, (hits.get(name) ?? 0) + 1);
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
    '--use-angle=swiftshader', '--disable-gpu-sandbox', '--no-sandbox', 'about:blank',
  ], { stdio: ['ignore', 'ignore', o.verbose ? 'inherit' : 'ignore'] });
  let ws = null;
  for (let i = 0; i < 100 && !ws; i++) {
    ws = await browserWsUrl(PORT + 1);
    if (!ws) await sleep(100);
  }
  if (!ws) throw new Error('chromium did not come up');
  return { server, chrome, cdp: await Cdp.connect(ws) };
}

/** The test in a page of its own (a fresh browser context, so its storage
 *  is its own too), every Worker's console relayed. */
async function runPage(test, arch, say, timeout) {
  const { browserContextId } = await cdp.send('Target.createBrowserContext', {});
  const { targetId } = await cdp.send('Target.createTarget', { url: 'about:blank', browserContextId });
  const { sessionId } = await cdp.send('Target.attachToTarget', { targetId, flatten: true });
  const names = new Map([[sessionId, 'page']]);
  const mine = new Set([sessionId]);
  const off = [];
  off.push(cdp.on('Runtime.consoleAPICalled', (p, sid) => {
    if (!mine.has(sid) || sid === sessionId) return; // the page's own lines come through __t
    const text = (p.args ?? []).map((a) => a.value ?? a.description ?? '').join(' ');
    if (o.verbose || /\b(FAIL|Error|error|trap|panic)\b/.test(text)) process.stderr.write(`${test.name}: ${names.get(sid)}: ${text}\n`);
  }));
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
  await cdp.send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: false, flatten: true }, sessionId);
  const q = new URLSearchParams({ test: test.name, src: `/@fs${test.file}`, arch });
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
  while (Date.now() - t0 < timeout) {
    await sleep(sent ? 50 : 250);
    const r = await cdp.send('Runtime.evaluate', { expression: poll, returnByValue: true }, sessionId).catch(() => null);
    const now = r?.result?.value ? JSON.parse(r.result.value) : null;
    if (!now) continue;
    v = now;
    for (; shown < v.lines.length; shown++) say(v.lines[shown]);
    // What the page asks of the browser (`t.cdp`): real input, which a user
    // gesture needs, and screenshots. Each answer goes back by its index.
    if (v.asks.length < sent) sent = v.asks.length; // a new phase
    for (; sent < v.asks.length; sent++) {
      const { method, save: name, ...params } = v.asks[sent];
      const reply = await cdp.send(method, params, sessionId).then(
        (result) => ({ result: name && result?.data ? { path: save(name, result.data) } : result }),
        (e) => ({ error: String(e?.message ?? e) }),
      );
      await cdp.send('Runtime.evaluate', { expression: `globalThis.__t.replies[${sent}] = ${JSON.stringify(reply)}` }, sessionId).catch(() => {});
    }
    if (v.done) break;
  }
  // What the page showed when it failed.
  if (!v?.done || v.result === 'FAIL') {
    const shot = await cdp.send('Page.captureScreenshot', { format: 'png' }, sessionId).catch(() => null);
    if (shot?.data) save('fail.png', shot.data);
  }
  for (const f of off) f();
  await cdp.send('Target.closeTarget', { targetId }).catch(() => {});
  await cdp.send('Target.disposeBrowserContext', { browserContextId }).catch(() => {});
  const res = v?.res ?? { checks: 0, fail: [], times: {} };
  if (!v?.done) {
    const what = `timed out after ${timeout / 1000} s`;
    say(`FAIL ${what}`);
    return { result: 'FAIL', ...res, fail: [...res.fail, what] };
  }
  return { result: v.result, skip: v.skip, ...res };
}
