// The site's check (correctness only, no timing): serve SITE (34) on
// 127.0.0.1, open it in headless Chromium (WebGPU) at a 1280x720 viewport,
// and check, from screenshots, that Windows 7's desktop comes up:
//   - the taskbar: Windows 7 Basic's blue bar along the bottom (y 680-719);
//   - the wallpaper (img0.jpg: blue sky, and its Windows logo's orange
//     pane, not Windows' plain default background);
//   - the desktop's Recycle Bin: its icon and its label, top left;
//   - the pins: Internet Explorer's, Windows Explorer's and Windows Media
//     Player's icons in the taskbar's first three slots after Start;
//   - the clock: the notification area's time and date text;
//   - the Start menu: a click on the Start button opens it (its white
//     program list over the desktop's left side).
// Writes OUT-desktop.png, OUT-start.png and OUT.log (the page's console).
//   node page-check.mjs SITE OUT [--port N] [--secs S] [--settle S]
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

const profile = mkdtempSync(join(tmpdir(), 'w7-check-'));
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
await browser.send('Page.enable', {}, page);
await browser.send('Emulation.setDeviceMetricsOverride', { width: 1280, height: 720, deviceScaleFactor: 1, mobile: false }, page);
writeFileSync(`${out}.log`, '');
const t0 = Date.now(), now = () => ((Date.now() - t0) / 1000).toFixed(1);
browser.on('Runtime.consoleAPICalled', (p) => log(`${now()} ${p.args.map((a) => a.value ?? a.description ?? '').join(' ')}`));
browser.on('Runtime.exceptionThrown', (p) => log(`EXC ${JSON.stringify(p.exceptionDetails).slice(0, 2000)}`));
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

// The taskbar: wait for it, then let the shell settle (the tray, the pins' icons).
const taskbar = (im) => im && im.w === 1280 && im.h === 720 && [400, 600, 800, 1000].every((x) => bar(im.at(x, 700)));
let im = null, seen = null;
while ((Date.now() - t0) / 1000 < secs) {
  im = await shot();
  if (taskbar(im)) { seen = now(); break; }
  await sleep(5000);
}
check(seen !== null, `the taskbar comes up (Basic's blue along the bottom)${seen ? ` at ${seen} s` : ` within ${secs} s`}`);
if (seen !== null) {
  log(`${now()} taskbar; settling ${settle} s`);
  await sleep(settle * 1000);
  im = await shot(`${out}-desktop.png`);
  check(taskbar(im), 'the taskbar is still up after settling');
  const logo = count(im, 515, 215, 670, 340, orange);
  check([[640, 60], [1000, 300], [60, 300]].every(([x, y]) => sky(im.at(x, y))) && logo >= 2000, `the desktop shows the wallpaper (${logo} logo pixels)`);
  const icon = count(im, 15, 8, 56, 54, bin), label = count(im, 4, 58, 66, 72, light);
  check(icon >= 100 && label >= 50, `the desktop shows the Recycle Bin (icon ${icon}, label ${label} pixels)`);
  // each pin: an icon of its own colours, not the bar's (slots 60 wide from x 60)
  const pins = [['Internet Explorer', 60], ['Windows Explorer', 120], ['Windows Media Player', 180]]
    .map(([name, x]) => [name, count(im, x + 12, 686, x + 48, 716, (p) => !bar(p))]);
  check(pins.every(([, n]) => n >= 150), `the taskbar shows the three pins (icon pixels: ${pins.map(([a, n]) => `${a} ${n}`).join(', ')})`);
  const clock = count(im, 1185, 684, 1262, 718, light);
  check(clock >= 40, `the clock shows the time and date (${clock} text pixels)`);
  await click(27, 700);
  await sleep(15000);
  const sm = await shot(`${out}-start.png`);
  const list = count(sm, 20, 300, 240, 600, white);
  check(list >= 20000, `the Start button opens the Start menu (${list} white pixels in its program list)`);
}
console.log(`page-check: ${fails.length ? `FAIL (${fails.length})` : 'PASS'}; log ${out}.log`);
done(fails.length ? 1 : 0);
