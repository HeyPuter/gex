// What tests/nt's browser drivers share (docs/nt-layer.md §10.2): a vite
// build of web/nt.html served cross-origin isolated, a headless Chromium driven over CDP (web/test/cdp.mjs)
// with every Worker's console relayed, web/nt.html opened on a bundle, and
// screenshots decoded for pixel checks.

import { spawn, execFileSync } from 'node:child_process';
import { existsSync, renameSync, mkdirSync, writeFileSync, rmSync, symlinkSync, statSync, createReadStream, readdirSync, openSync, readFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { join, dirname, normalize, extname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { inflateSync } from 'node:zlib';
import { Cdp, browserWsUrl } from '../../web/test/cdp.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
export const ROOT = join(HERE, '..', '..');
export const WEB = join(ROOT, 'web');
export const PREFIX = process.env.GEX_WINE_PREFIX || join(process.env.HOME, '.cache/gex/wine-11.15');
const CHROME = process.env.CHROME || '/usr/bin/chromium';
const VITE_PORT = Number(process.env.GEX_NT_VITE_PORT || 5199);
const CHROME_PORT = Number(process.env.GEX_NT_CHROME_PORT || 9339);
export const STATE = join(process.env.TMPDIR || '/tmp', `gex-nt-browser-${process.getuid?.() ?? 0}`);
mkdirSync(STATE, { recursive: true });

export const log = (s) => process.stderr.write(`[nt-browser] ${s}\n`);
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/**
 * A tar for web/nt.html: the prefix under /wine (with only this
 * architecture's system directory: PE32+ needs system32, PE32 syswow64) and
 * `trees` -- `{ guestDir: hostDir }` -- beside it. Rebuilt when any `deps`
 * file is newer.
 */
export function bundle(name, arch, trees, deps = []) {
  const out = join(WEB, 'public/gex/nt', `${name}.tar`);
  const newest = Math.max(0, ...deps.map((d) => statSync(d).mtimeMs));
  if (existsSync(out) && statSync(out).mtimeMs > newest) return out;
  mkdirSync(dirname(out), { recursive: true });
  const stage = join(STATE, `stage-${name}`);
  rmSync(stage, { recursive: true, force: true });
  mkdirSync(stage, { recursive: true });
  symlinkSync(PREFIX, join(stage, 'wine'));
  const top = ['wine'];
  for (const [guest, host] of Object.entries(trees)) {
    const at = join(stage, guest);
    mkdirSync(dirname(at), { recursive: true });
    symlinkSync(host, at);
    top.push(guest.split('/')[0]);
  }
  const skip = arch === 'x64' ? 'wine/drive_c/windows/syswow64' : 'wine/drive_c/windows/system32';
  // Through a temporary name: a failed tar must not leave a bundle that
  // looks newer than its inputs.
  execFileSync('tar', ['-chf', `${out}.tmp`, '-C', stage, `--exclude=${skip}`, ...new Set(top)]);
  renameSync(`${out}.tmp`, out);
  log(`bundle ${out}: ${(statSync(out).size / 1048576).toFixed(0)} MiB`);
  return out;
}

// -- PNG, just enough for a screenshot ------------------------------------------

export function decodePng(buf) {
  let at = 8;
  let w = 0, h = 0, type = 0;
  const idat = [];
  while (at < buf.length) {
    const len = buf.readUInt32BE(at);
    const kind = buf.toString('latin1', at + 4, at + 8);
    const data = buf.subarray(at + 8, at + 8 + len);
    if (kind === 'IHDR') {
      w = data.readUInt32BE(0);
      h = data.readUInt32BE(4);
      if (data[8] !== 8 || data[12] !== 0) throw new Error('only 8-bit non-interlaced PNGs');
      type = data[9];
    } else if (kind === 'IDAT') idat.push(data);
    at += 12 + len;
  }
  const bpp = type === 6 ? 4 : type === 2 ? 3 : 0;
  if (!bpp) throw new Error(`PNG colour type ${type}`);
  const raw = inflateSync(Buffer.concat(idat));
  const px = Buffer.alloc(w * h * bpp);
  const stride = w * bpp;
  for (let y = 0; y < h; y++) {
    const f = raw[y * (stride + 1)];
    const src = raw.subarray(y * (stride + 1) + 1, (y + 1) * (stride + 1));
    for (let x = 0; x < stride; x++) {
      const a = x >= bpp ? px[y * stride + x - bpp] : 0;
      const b = y > 0 ? px[(y - 1) * stride + x] : 0;
      const c = x >= bpp && y > 0 ? px[(y - 1) * stride + x - bpp] : 0;
      let v = src[x];
      if (f === 1) v += a;
      else if (f === 2) v += b;
      else if (f === 3) v += (a + b) >> 1;
      else if (f === 4) {
        const p = a + b - c;
        const pa = Math.abs(p - a), pb = Math.abs(p - b), pc = Math.abs(p - c);
        v += pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
      }
      px[y * stride + x] = v & 0xff;
    }
  }
  return { w, h, at: (x, y) => [...px.subarray((y * w + x) * bpp, (y * w + x) * bpp + 3)] };
}

export const near = (got, want, tol = 24) => got.every((v, i) => Math.abs(v - want[i]) <= tol);


const children = [];
const servers = [];


/**
 * Build web/nt.html with vite (the production build: the dev server's
 * module rewriting breaks the compositor worker's Mesa glue) into STATE/dist,
 * and serve it, cross-origin isolated, with web/public behind it -- not
 * copied, since a bundle can be gigabytes.
 */
async function serveBuilt() {
  if (!existsSync(join(WEB, 'node_modules/.bin/vite'))) throw new Error('no web/node_modules (vite)');
  const dist = join(STATE, 'dist');
  const config = join(STATE, 'vite.nt.config.mjs');
  writeFileSync(config, `import base from ${JSON.stringify(join(WEB, 'vite.config.js'))};
export default {
  ...base,
  root: ${JSON.stringify(WEB)},
  publicDir: false,
  logLevel: 'warn',
  build: { ...base.build, outDir: ${JSON.stringify(dist)}, emptyOutDir: true,
           rollupOptions: { input: { nt: ${JSON.stringify(join(WEB, 'nt.html'))} } } },
};
`);
  execFileSync(join(WEB, 'node_modules/.bin/vite'), ['build', '--config', config], { cwd: WEB, stdio: ['ignore', 'ignore', 'inherit'] });
  const types = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.wasm': 'application/wasm',
    '.css': 'text/css', '.json': 'application/json', '.tar': 'application/x-tar' };
  const server = createServer((req, res) => {
    // A Worker's dump (gex.mjs: the bytes of a module the engine refused),
    // into STATE/dumps, a few at most.
    // A GL recording from the compositor Worker (?glcapture=BYTES), to
    // STATE/capture.gexgltr.
    if (req.method === 'POST' && req.url.startsWith('/__gex_capture')) {
      const chunks = [];
      req.on('data', (c) => chunks.push(c));
      req.on('end', () => {
        writeFileSync(join(STATE, 'capture.gexgltr'), Buffer.concat(chunks));
        log(`GL capture: ${join(STATE, 'capture.gexgltr')}`);
        res.writeHead(204, { 'Cross-Origin-Resource-Policy': 'same-origin' });
        res.end();
      });
      return;
    }
    if (req.method === 'POST' && req.url.startsWith('/__gex_dump')) {
      const chunks = [];
      req.on('data', (c) => chunks.push(c));
      req.on('end', () => {
        const dir = join(STATE, 'dumps');
        mkdirSync(dir, { recursive: true });
        if (readdirSync(dir).length < 8) {
          const f = join(dir, `${Date.now()}-${new URL(req.url, 'http://x').searchParams.get('name') ?? 'dump'}.wasm`);
          writeFileSync(f, Buffer.concat(chunks));
          log(`dump: ${f}`);
        }
        res.writeHead(204, { 'Cross-Origin-Resource-Policy': 'same-origin' });
        res.end();
      });
      return;
    }
    const rel = normalize(decodeURIComponent(new URL(req.url, 'http://x').pathname)).replace(/^(\.\.[/\\])+/, '');
    // `/gex-cgen/...`: the function-tier unit cache (GEX_CGEN_CACHE), for
    // a page opened with `cgen=1|2`.
    const cgen = process.env.GEX_CGEN_CACHE && rel.startsWith('/gex-cgen/')
      ? join(process.env.GEX_CGEN_CACHE, rel.slice('/gex-cgen/'.length)) : null;
    const file = (cgen ? [cgen] : [join(dist, rel), join(WEB, 'public', rel)]).find((f) => existsSync(f) && statSync(f).isFile());
    if (!file) {
      res.writeHead(404);
      res.end();
      return;
    }
    res.writeHead(200, {
      'Content-Type': types[extname(file)] || 'application/octet-stream',
      'Content-Length': statSync(file).size,
      'Cross-Origin-Opener-Policy': 'same-origin',
      'Cross-Origin-Embedder-Policy': 'require-corp',
      'Cache-Control': 'no-cache',
    });
    createReadStream(file).pipe(res);
  });
  await new Promise((r) => server.listen(VITE_PORT, '127.0.0.1', r));
  servers.push(server);
}

/**
 * Serve the page and start Chromium, open web/nt.html with `query`, and hand back the
 * means to watch it. Software GL and WebGPU unless GEX_WEBGPU_SOFTWARE=0.
 */
export async function openPage(query, { width = 1280, height = 900, shotDir = null } = {}) {
  for (const f of ['gex_browser.x86.shared.wasm', 'seed.x86.shared.wasm', 'gex_kernel.x86.wasm']) {
    if (!existsSync(join(WEB, 'public/gex', f))) throw new Error(`no web/public/gex/${f}`);
  }
  // A page with OpenGL needs Mesa's wasm, and without it the GL device
  // simply never answers ("did not answer in thirty seconds"): say which
  // file is missing and how to make it instead.
  if (/(^|&)gl=1(&|$)/.test(query)) {
    for (const f of ['gex-wgpu-replay.js', 'gex-wgpu-replay.wasm']) {
      if (!existsSync(join(WEB, 'public/gex', f))) {
        throw new Error(`no web/public/gex/${f}: the page's OpenGL is Mesa built to wasm; build it with web/build-mesa-wasm.sh`);
      }
    }
  }
  await serveBuilt();
  if (await browserWsUrl(CHROME_PORT)) {
    throw new Error(`a browser is already listening on ${CHROME_PORT} (a run left behind?); stop it first`);
  }
  const soft = process.env.GEX_WEBGPU_SOFTWARE !== '0';
  const chrome = spawn(CHROME, [
    '--headless', '--no-sandbox', '--disable-gpu-sandbox', '--enable-unsafe-swiftshader',
    '--enable-unsafe-webgpu',
    ...(soft ? ['--use-angle=swiftshader-webgl', '--use-webgpu-adapter=swiftshader'] : ['--use-angle=vulkan', '--enable-features=Vulkan']),
    '--ozone-platform=headless', `--ozone-override-screen-size=${width},${height}`, `--window-size=${width},${height}`,
    ...(process.env.GEX_NT_CHROME_FLAGS ? process.env.GEX_NT_CHROME_FLAGS.split(' ') : []),
    '--enable-logging=stderr', '--disable-hang-monitor', '--no-first-run', '--no-default-browser-check', '--remote-allow-origins=*',
    `--remote-debugging-port=${CHROME_PORT}`, `--user-data-dir=${join(STATE, 'profile')}`, 'about:blank',
  ], { detached: true, stdio: ['ignore',
    // V8's traces (GEX_NT_CHROME_FLAGS=--js-flags=--trace-wasm-inlining) print to stdout.
    process.env.GEX_NT_CHROME_STDOUT ? openSync(process.env.GEX_NT_CHROME_STDOUT, 'w') : 'ignore',
    openSync(join(STATE, 'chrome.log'), 'w')] });
  children.push(chrome);
  let ws = null;
  for (let i = 0; i < 80 && !ws; i++) {
    ws = await browserWsUrl(CHROME_PORT);
    if (!ws) await sleep(250);
  }
  if (!ws) throw new Error('chromium did not start');
  const browser = await Cdp.connect(ws);
  const { targetId } = await browser.send('Target.createTarget', { url: 'about:blank' });
  const { sessionId: page } = await browser.send('Target.attachToTarget', { targetId, flatten: true });
  const consoleLines = [];
  let printed = 0;
  // Every Worker's console too: each guest thread is one.
  browser.on('Runtime.consoleAPICalled', (p, sid) => {
    const text = p.args.map((a) => a.value ?? a.description ?? '').join(' ');
    consoleLines.push(text);
    if (consoleLines.length > 5000) consoleLines.shift();
    // Errors, and the runtime's own warnings -- not every warning: Mesa's
    // per-call notes run to gigabytes over a game's loading screen. Capped
    // too, since a log that fills the disk ends more than the run.
    const wanted = p.type === 'error'
      ? !/^\[mesa-gl\] gex-gl: gl[A-Z]/.test(text)
      : p.type === 'warning' && /gex-nt|gex-wgpu|wine:|\[gex\]|panic|trap/i.test(text);
    if ((wanted || process.env.GEX_NT_BROWSER_VERBOSE) && printed++ < 20000) {
      log(`console[${sid?.slice(0, 6)}] ${text.slice(0, 4000)}`);
    }
  });
  browser.on('Runtime.exceptionThrown', (p, sid) => log(`exception[${sid?.slice(0, 6)}] ${p.exceptionDetails?.exception?.description ?? p.exceptionDetails?.text}`));
  // A crashed renderer answers nothing again: say so, with Chromium's own
  // last words (STATE/chrome.log), and let the driver stop.
  browser.on('Inspector.targetCrashed', () => {
    log('the page CRASHED');
    consoleLines.push('[harness] the page crashed');
    try {
      const tail = readFileSync(join(STATE, 'chrome.log'), 'utf8').split('\n').slice(-15).join('\n');
      log(`chrome.log ends:\n${tail}`);
    } catch {}
  });
  // Every attached target (the Workers), by session: for `profile`.
  const targets = new Map();
  browser.on('Target.attachedToTarget', async (p) => {
    targets.set(p.sessionId, p.targetInfo?.url ?? '');
    await browser.send('Runtime.enable', {}, p.sessionId).catch(() => {});
    await browser.send('Runtime.runIfWaitingForDebugger', {}, p.sessionId).catch(() => {});
  });
  await browser.send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: false, flatten: true }, page);
  await browser.send('Runtime.enable', {}, page);
  await browser.send('Page.enable', {}, page);
  await browser.send('Inspector.enable', {}, page).catch(() => {});
  await browser.send('Emulation.setDeviceMetricsOverride', { width, height, deviceScaleFactor: 1, mobile: false }, page);

  const evaluate = async (expr, awaitPromise = false) => {
    const r = await browser.send('Runtime.evaluate', { expression: expr, returnByValue: true, awaitPromise }, page);
    return r.result?.value;
  };
  const state = async () => {
    const v = await evaluate('JSON.stringify(globalThis.__nt ?? null)');
    return v ? JSON.parse(v) : null;
  };
  const until = async (test, ms, what) => {
    const end = Date.now() + ms;
    for (;;) {
      const s = await state();
      if (s && test(s)) return s;
      if (s?.phase === 'failed') throw new Error(`the page failed: ${s.error}`);
      if (Date.now() > end) throw new Error(`timed out waiting for ${what}; output so far: ${JSON.stringify(s?.out ?? '')}`);
      await sleep(250);
    }
  };
  const shot = async (name) => {
    const { data } = await browser.send('Page.captureScreenshot', { format: 'png' }, page);
    const buf = Buffer.from(data, 'base64');
    if (shotDir) {
      mkdirSync(shotDir, { recursive: true });
      writeFileSync(join(shotDir, `${name}.png`), buf);
    }
    return decodePng(buf);
  };
  // SwiftShader's WebGPU loses its device when the NT host hands the
  // compositor GL frames directly (the canvas present path), so with the
  // software adapter a Windows program's frames are read back instead.
  if (soft && !/GEX_NT_GL_READBACK/.test(query)) {
    query = /(^|&)env=/.test(query)
      ? query.replace(/((?:^|&)env=)/, '$1GEX_NT_GL_READBACK=1,')
      : `${query}&env=GEX_NT_GL_READBACK=1`;
  }
  const url = `http://127.0.0.1:${VITE_PORT}/nt.html?${query}`;
  log(`open ${url}`);
  await browser.send('Page.navigate', { url }, page);
  // A CPU profile of each Worker whose script URL contains `match`, over
  // `ms`: [{ url, profile }] (Chrome's .cpuprofile shape).
  // `withPage`: the page's own (main) thread too, first, as url 'page'.
  const profile = async (match, ms, withPage = false) => {
    const sids = [...(withPage ? [page] : []), ...[...targets].filter(([, u]) => u.includes(match)).map(([s]) => s)];
    for (const s of sids) {
      await browser.send('Profiler.enable', {}, s);
      await browser.send('Profiler.setSamplingInterval', { interval: 250 }, s);
      await browser.send('Profiler.start', {}, s);
    }
    await sleep(ms);
    const out = [];
    for (const s of sids) {
      const { profile } = await browser.send('Profiler.stop', {}, s);
      out.push({ url: s === page ? 'page' : targets.get(s), profile });
    }
    return out;
  };
  return { browser, page, evaluate, state, until, shot, consoleLines, profile };
}

/** A CPU profile summarised: the `n` functions with the most self time and
 *  the `n` with the most inclusive time, in ms and percent of the span. */
export function summariseProfile(profile, n = 25) {
  const byId = new Map(profile.nodes.map((x) => [x.id, x]));
  const parent = new Map();
  for (const x of profile.nodes) for (const c of x.children ?? []) parent.set(c, x.id);
  const name = (x) => {
    const f = x.callFrame;
    const file = (f.url || '').split('/').pop().split('?')[0];
    return `${f.functionName || '(anonymous)'}${file ? ` [${file}]` : ''}`;
  };
  const self = new Map();
  const incl = new Map();
  let total = 0;
  const { samples = [], timeDeltas = [] } = profile;
  for (let i = 0; i < samples.length; i++) {
    // The time to the next sample is this one's.
    const dt = (timeDeltas[i + 1] ?? 0) / 1000;
    total += dt;
    let x = byId.get(samples[i]);
    const k = name(x);
    self.set(k, (self.get(k) ?? 0) + dt);
    const seen = new Set();
    for (; x; x = byId.get(parent.get(x.id))) {
      const kk = name(x);
      if (seen.has(kk)) continue;
      seen.add(kk);
      incl.set(kk, (incl.get(kk) ?? 0) + dt);
    }
  }
  const top = (m) => [...m].sort((a, b) => b[1] - a[1]).slice(0, n)
    .map(([k, v]) => `  ${v.toFixed(0).padStart(7)} ms ${(100 * v / Math.max(total, 1e-9)).toFixed(1).padStart(5)}%  ${k}`).join('\n');
  return `span ${total.toFixed(0)} ms\nself:\n${top(self)}\ninclusive:\n${top(incl)}`;
}

/** Stop vite and Chromium. */
export function stopAll() {
  for (const c of children) {
    try {
      process.kill(-c.pid, 'SIGKILL');
    } catch {}
  }
  children.length = 0;
  for (const s of servers) s.close();
  servers.length = 0;
}
process.on('exit', stopAll);
