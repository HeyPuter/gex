// What tests/nt's browser drivers share (docs/nt-layer.md §10.2): a vite
// build of web/nt.html served cross-origin isolated, a headless Chromium driven over CDP (web/test/cdp.mjs)
// with every Worker's console relayed, web/nt.html opened on a bundle, and
// screenshots decoded for pixel checks.

import { spawn, execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, writeFileSync, rmSync, symlinkSync, statSync, createReadStream } from 'node:fs';
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
  execFileSync('tar', ['-chf', out, '-C', stage, `--exclude=${skip}`, ...new Set(top)]);
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
    const rel = normalize(decodeURIComponent(new URL(req.url, 'http://x').pathname)).replace(/^(\.\.[/\\])+/, '');
    const file = [join(dist, rel), join(WEB, 'public', rel)].find((f) => existsSync(f) && statSync(f).isFile());
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
  await serveBuilt();
  const soft = process.env.GEX_WEBGPU_SOFTWARE !== '0';
  const chrome = spawn(CHROME, [
    '--headless', '--no-sandbox', '--disable-gpu-sandbox', '--enable-unsafe-swiftshader',
    '--enable-unsafe-webgpu',
    ...(soft ? ['--use-angle=swiftshader-webgl', '--use-webgpu-adapter=swiftshader'] : ['--use-angle=vulkan', '--enable-features=Vulkan']),
    '--ozone-platform=headless', `--ozone-override-screen-size=${width},${height}`, `--window-size=${width},${height}`,
    '--disable-hang-monitor', '--no-first-run', '--no-default-browser-check', '--remote-allow-origins=*',
    `--remote-debugging-port=${CHROME_PORT}`, `--user-data-dir=${join(STATE, 'profile')}`, 'about:blank',
  ], { detached: true, stdio: ['ignore', 'ignore', 'ignore'] });
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
  // Every Worker's console too: each guest thread is one.
  browser.on('Runtime.consoleAPICalled', (p, sid) => {
    const text = p.args.map((a) => a.value ?? a.description ?? '').join(' ');
    consoleLines.push(text);
    if (consoleLines.length > 5000) consoleLines.shift();
    if (p.type === 'error' || p.type === 'warning' || process.env.GEX_NT_BROWSER_VERBOSE) log(`console[${sid?.slice(0, 6)}] ${text.slice(0, 400)}`);
  });
  browser.on('Runtime.exceptionThrown', (p, sid) => log(`exception[${sid?.slice(0, 6)}] ${p.exceptionDetails?.exception?.description ?? p.exceptionDetails?.text}`));
  browser.on('Inspector.targetCrashed', () => log('the page CRASHED (likely out of memory)'));
  browser.on('Target.attachedToTarget', async (p) => {
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
  const url = `http://127.0.0.1:${VITE_PORT}/nt.html?${query}`;
  log(`open ${url}`);
  await browser.send('Page.navigate', { url }, page);
  return { browser, page, evaluate, state, until, shot, consoleLines };
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
