// Start a program from the site's shell and watch it (correctness only, no
// timing): serve SITE, open it in headless Chromium at 1280x720 as
// page-check.mjs does, wait for the taskbar, then click each point of
// --clicks in turn (e.g. the Start button, then a Start menu entry) and take
// a screenshot every --every seconds for --wait seconds.
// Writes OUT-NN.png and OUT.log (the page's console).
//   node page-launch.mjs SITE OUT --clicks 'X,Y;X,Y' [--wait S] [--every S] [--settle S]
//   env CHROME (default /usr/bin/chromium)
import { spawn } from 'node:child_process';
import { appendFileSync, createReadStream, mkdtempSync, rmSync, statSync, writeFileSync } from 'node:fs';
import http from 'node:http';
import { tmpdir } from 'node:os';
import { dirname, extname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { inflateSync } from 'node:zlib';
const { Cdp, browserWsUrl } = await import(join(dirname(fileURLToPath(import.meta.url)), '../../web/test/cdp.mjs'));

const av = process.argv.slice(2);
const opt = (n, d) => { const i = av.indexOf(n); return i >= 0 ? av[i + 1] : d; };
const site = resolve(av[0]), out = resolve(av[1]);
const port = Number(opt('--port', '8791')), secs = Number(opt('--secs', '900')), settle = Number(opt('--settle', '90'));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const log = (t) => appendFileSync(`${out}.log`, `${t}\n`);

// The site, with the headers SharedArrayBuffer needs, and byte ranges (the lazy files).
const types = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.wasm': 'application/wasm', '.json': 'application/json', '.css': 'text/css' };
const server = http.createServer((req, res) => {
  let p = join(site, decodeURIComponent(new URL(req.url, 'http://x').pathname));
  const h = { 'Cross-Origin-Opener-Policy': 'same-origin', 'Cross-Origin-Embedder-Policy': 'require-corp', 'Cross-Origin-Resource-Policy': 'same-origin' };
  if (!p.startsWith(site)) { res.writeHead(403, h).end(); return; }
  let st = null;
  try { st = statSync(p); if (st.isDirectory()) { p = join(p, 'index.html'); st = statSync(p); } } catch { st = null; }
  if (!st?.isFile()) { res.writeHead(404, h).end(); return; }
  const r = /^bytes=(\d+)-(\d*)$/.exec(req.headers.range ?? '');
  if (r) {
    const a = Number(r[1]), b = Math.min(r[2] ? Number(r[2]) : st.size - 1, st.size - 1);
    res.writeHead(206, { ...h, 'Content-Type': 'application/octet-stream', 'Content-Length': b - a + 1, 'Content-Range': `bytes ${a}-${b}/${st.size}` });
    createReadStream(p, { start: a, end: b }).pipe(res);
    return;
  }
  res.writeHead(200, { ...h, 'Content-Type': types[extname(p)] || 'application/octet-stream', 'Content-Length': st.size });
  createReadStream(p).pipe(res);
});
await new Promise((r) => server.listen(port, '127.0.0.1', r));

// A screenshot's pixels: an 8-bit RGB or RGBA PNG, unfiltered.
function png(buf) {
  let o = 8, w = 0, h = 0, ch = 0;
  const idat = [];
  while (o < buf.length) {
    const len = buf.readUInt32BE(o), type = buf.toString('latin1', o + 4, o + 8), body = buf.subarray(o + 8, o + 8 + len);
    if (type === 'IHDR') { w = body.readUInt32BE(0); h = body.readUInt32BE(4); ch = body[9] === 6 ? 4 : 3; if (body[8] !== 8 || body[12]) throw new Error('unsupported PNG'); }
    if (type === 'IDAT') idat.push(body);
    o += 12 + len;
  }
  const raw = inflateSync(Buffer.concat(idat)), stride = w * ch, px = Buffer.alloc(h * stride);
  for (let y = 0; y < h; y++) {
    const f = raw[y * (stride + 1)], src = raw.subarray(y * (stride + 1) + 1, (y + 1) * (stride + 1)), row = y * stride;
    for (let x = 0; x < stride; x++) {
      const a = x >= ch ? px[row + x - ch] : 0, b = y ? px[row - stride + x] : 0, c = x >= ch && y ? px[row - stride + x - ch] : 0;
      let v = src[x];
      if (f === 1) v += a; else if (f === 2) v += b; else if (f === 3) v += (a + b) >> 1;
      else if (f === 4) { const p = a + b - c, pa = Math.abs(p - a), pb = Math.abs(p - b), pc = Math.abs(p - c); v += pa <= pb && pa <= pc ? a : pb <= pc ? b : c; }
      px[row + x] = v & 255;
    }
  }
  return { w, h, at: (x, y) => [px[y * stride + x * ch], px[y * stride + x * ch + 1], px[y * stride + x * ch + 2]] };
}
const count = (im, x0, y0, x1, y1, f) => { let n = 0; for (let y = y0; y < y1; y++) for (let x = x0; x < x1; x++) if (f(im.at(x, y))) n++; return n; };
const bar = (p) => p[2] >= 180 && p[2] - p[0] >= 30;               // Basic's taskbar blue
const sky = (p) => p[2] >= 150 && p[2] > p[0] + 40;                 // img0.jpg's sky
const light = (p) => Math.min(...p) >= 215;                         // the clock's text (light on the tray's grey)
const white = (p) => p[0] >= 250 && p[1] >= 250 && p[2] >= 250;
const orange = (p) => p[0] >= 200 && p[1] < 170 && p[2] < 120;      // the logo's top left pane
const bin = (p) => p[0] >= 120 && Math.abs(p[0] - p[2]) < 40;       // the Recycle Bin's grey basket
const fails = [];
const check = (ok, what) => { const l = `${ok ? 'ok  ' : 'FAIL'} ${what}`; console.log(l); log(l); if (!ok) fails.push(what); };

const profile = mkdtempSync(join(tmpdir(), 'w7-launch-'));
const chrome = spawn(process.env.CHROME || '/usr/bin/chromium', [
  '--headless', '--no-sandbox', '--disable-gpu-sandbox', '--enable-unsafe-webgpu', '--use-angle=vulkan', '--enable-features=Vulkan',
  '--ozone-platform=headless', '--ozone-override-screen-size=1280,720', '--window-size=1280,720', '--disable-hang-monitor',
  '--no-first-run', '--no-default-browser-check', '--remote-allow-origins=*', `--remote-debugging-port=${port + 1}`, `--user-data-dir=${profile}`, 'about:blank',
], { detached: true, stdio: 'ignore' });
const done = (code) => { try { process.kill(-chrome.pid, 'SIGKILL'); } catch {} server.close(); rmSync(profile, { recursive: true, force: true }); process.exit(code); };
let ws = null;
for (let i = 0; i < 80 && !ws; i++) { ws = await browserWsUrl(port + 1); if (!ws) await sleep(250); }
if (!ws) { console.log('page-check: no Chromium'); done(2); }
const browser = await Cdp.connect(ws);
const { targetId } = await browser.send('Target.createTarget', { url: 'about:blank' });
const { sessionId: page } = await browser.send('Target.attachToTarget', { targetId, flatten: true });
await browser.send('Runtime.enable', {}, page);
browser.on('Runtime.consoleAPICalled', (p) => log(`${now()} ${p.args.map((a) => a.value ?? a.description ?? '').join(' ')}`));
browser.on('Runtime.exceptionThrown', (p) => log(`EXC ${JSON.stringify(p.exceptionDetails).slice(0, 2000)}`));
await browser.send('Page.enable', {}, page);
await browser.send('Emulation.setDeviceMetricsOverride', { width: 1280, height: 720, deviceScaleFactor: 1, mobile: false }, page);
writeFileSync(`${out}.log`, '');
const t0 = Date.now(), now = () => ((Date.now() - t0) / 1000).toFixed(1);
const shot = async (f) => {
  const r = await browser.send('Page.captureScreenshot', { format: 'png' }, page).catch(() => null);
  if (!r) return null;
  const b = Buffer.from(r.data, 'base64');
  if (f) writeFileSync(f, b);
  return png(b);
};
const click = async (x, y) => {
  for (const type of ['mouseMoved', 'mousePressed', 'mouseReleased']) {
    await browser.send('Input.dispatchMouseEvent', { type, x, y, button: type === 'mouseMoved' ? 'none' : 'left', buttons: type === 'mousePressed' ? 1 : 0, clickCount: 1 }, page);
    await sleep(80);
  }
};
await browser.send('Page.navigate', { url: `http://127.0.0.1:${port}/` }, page);
const taskbar = (im) => im && im.w === 1280 && im.h === 720 && [400, 600, 800, 1000].every((x) => bar(im.at(x, 700)));
let seen = null;
while ((Date.now() - t0) / 1000 < secs) {
  if (taskbar(await shot())) { seen = now(); break; }
  await sleep(5000);
}
if (seen === null) { console.log('page-launch: no taskbar'); done(1); }
log(`${now()} taskbar; settling ${settle} s`);
console.log(`taskbar at ${seen} s`);
await sleep(settle * 1000);
for (const c of opt('--clicks', '').split(';').filter(Boolean)) {
  const [x, y] = c.split(',').map(Number);
  log(`${now()} click ${x},${y}`);
  await click(x, y);
  await sleep(3000);
}
const wait = Number(opt('--wait', '90')), every = Number(opt('--every', '15'));
for (let i = 0; i * every <= wait; i++) {
  await shot(`${out}-${String(i).padStart(2, '0')}.png`);
  if ((i + 1) * every <= wait) await sleep(every * 1000);
}
console.log(`page-launch: done; ${out}-NN.png, ${out}.log`);
done(0);
