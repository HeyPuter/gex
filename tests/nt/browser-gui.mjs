// A Windows program's window in a real browser (docs/nt-layer.md §10.2,
// milestone (b)): tests/nt/bin/<arch>/window.exe on gex's NT layer in the
// shared-memory browser shim, its window a toplevel in the page's compositor
// (web/nt.html), checked by pixels and by input.
//
//   node tests/nt/browser-gui.mjs [--arch x64|x86] [--keep] [--shot DIR]
//
// Needs: the x86 shared shim, seed and kernel in web/public/gex
// (GEX_ARCH=x86 shims/browser/build.sh), web/node_modules (vite), a staged
// Wine prefix, and chromium. It starts its own vite dev server and headless
// chromium (software GL and WebGPU unless GEX_WEBGPU_SOFTWARE=0) and stops
// both at the end.
//
// The checks: the window shows its four quadrants and white centre; a click
// in the yellow quadrant reaches the program (it prints "click X Y") and its
// black mark appears there; Escape ends it with status 0.

import { spawn, execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, writeFileSync, rmSync, symlinkSync, readdirSync, statSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { inflateSync } from 'node:zlib';
import { Cdp, browserWsUrl } from '../../web/test/cdp.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..', '..');
const WEB = join(ROOT, 'web');
const argv = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = argv.indexOf(name);
  return i >= 0 ? argv[i + 1] : dflt;
};
const arch = opt('--arch', 'x64');
const keep = argv.includes('--keep');
const shotDir = opt('--shot', null);
const prefix = process.env.GEX_WINE_PREFIX || join(process.env.HOME, '.cache/gex/wine-11.15');
const CHROME = process.env.CHROME || '/usr/bin/chromium';
const VITE_PORT = Number(process.env.GEX_NT_VITE_PORT || 5199);
const CHROME_PORT = Number(process.env.GEX_NT_CHROME_PORT || 9339);
const STATE = join(process.env.TMPDIR || '/tmp', `gex-nt-gui-${process.getuid?.() ?? 0}`);
mkdirSync(STATE, { recursive: true });

const log = (s) => process.stderr.write(`[nt-gui] ${s}\n`);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failed = 0;
const check = (name, ok, detail = '') => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${name}${detail ? ` (${detail})` : ''}`);
  if (!ok) failed++;
};

// -- the bundle ---------------------------------------------------------------

/** A tar of the prefix (this architecture's system directory only) and the program. */
function bundle() {
  const out = join(WEB, 'public/gex/nt', `bundle-${arch}.tar`);
  const exe = join(HERE, 'bin', arch, 'window.exe');
  if (existsSync(out) && statSync(out).mtimeMs > statSync(exe).mtimeMs) return out;
  mkdirSync(dirname(out), { recursive: true });
  const stage = join(STATE, `stage-${arch}`);
  rmSync(stage, { recursive: true, force: true });
  mkdirSync(join(stage, 't'), { recursive: true });
  symlinkSync(prefix, join(stage, 'wine'));
  symlinkSync(exe, join(stage, 't', 'window.exe'));
  // PE32+ needs system32, PE32 syswow64 (and nothing else of the other).
  const skip = arch === 'x64' ? 'wine/drive_c/windows/syswow64' : 'wine/drive_c/windows/system32';
  execFileSync('tar', ['-chf', out, '-C', stage, `--exclude=${skip}`, 'wine', 't']);
  log(`bundle ${out}: ${(statSync(out).size / 1048576).toFixed(0)} MiB`);
  return out;
}

// -- PNG, just enough for a screenshot ------------------------------------------

function decodePng(buf) {
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

const near = (got, want, tol = 24) => got.every((v, i) => Math.abs(v - want[i]) <= tol);

// -- the run ------------------------------------------------------------------

async function waitHttp(url, ms) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    try {
      const r = await fetch(url, { signal: AbortSignal.timeout(1000) });
      if (r.ok) return true;
    } catch {}
    await sleep(250);
  }
  return false;
}

const children = [];
function stopAll() {
  for (const c of children) {
    try {
      process.kill(-c.pid, 'SIGKILL');
    } catch {}
  }
}
process.on('exit', stopAll);

async function main() {
  for (const f of ['gex_browser.x86.shared.wasm', 'seed.x86.shared.wasm', 'gex_kernel.x86.wasm']) {
    if (!existsSync(join(WEB, 'public/gex', f))) throw new Error(`no web/public/gex/${f}`);
  }
  if (!existsSync(join(WEB, 'node_modules/.bin/vite'))) throw new Error('no web/node_modules (vite)');
  const tar = bundle();

  const vite = spawn(join(WEB, 'node_modules/.bin/vite'), ['--port', String(VITE_PORT), '--strictPort', '--host', '127.0.0.1'], {
    cwd: WEB,
    detached: true,
    stdio: ['ignore', 'ignore', 'inherit'],
  });
  children.push(vite);
  if (!(await waitHttp(`http://127.0.0.1:${VITE_PORT}/nt.html`, 30000))) throw new Error('vite did not start');

  const soft = process.env.GEX_WEBGPU_SOFTWARE !== '0';
  const chrome = spawn(CHROME, [
    '--headless', '--no-sandbox', '--disable-gpu-sandbox', '--enable-unsafe-swiftshader',
    '--enable-unsafe-webgpu',
    ...(soft ? ['--use-angle=swiftshader-webgl', '--use-webgpu-adapter=swiftshader'] : ['--use-angle=vulkan', '--enable-features=Vulkan']),
    '--ozone-platform=headless', '--ozone-override-screen-size=1280,900', '--window-size=1280,900',
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
  // Every Worker's console too: each guest thread is one.
  browser.on('Runtime.consoleAPICalled', (p, sid) => {
    const text = p.args.map((a) => a.value ?? a.description ?? '').join(' ');
    if (p.type === 'error' || p.type === 'warning' || process.env.GEX_NT_GUI_VERBOSE) log(`console[${sid?.slice(0, 6)}] ${text.slice(0, 400)}`);
  });
  browser.on('Runtime.exceptionThrown', (p, sid) => log(`exception[${sid?.slice(0, 6)}] ${p.exceptionDetails?.exception?.description ?? p.exceptionDetails?.text}`));
  browser.on('Target.attachedToTarget', async (p) => {
    await browser.send('Runtime.enable', {}, p.sessionId).catch(() => {});
    await browser.send('Runtime.runIfWaitingForDebugger', {}, p.sessionId).catch(() => {});
  });
  await browser.send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: false, flatten: true }, page);
  await browser.send('Runtime.enable', {}, page);
  await browser.send('Page.enable', {}, page);
  await browser.send('Emulation.setDeviceMetricsOverride', { width: 1280, height: 900, deviceScaleFactor: 1, mobile: false }, page);

  const eval_ = async (expr, awaitPromise = false) => {
    const r = await browser.send('Runtime.evaluate', { expression: expr, returnByValue: true, awaitPromise }, page);
    return r.result?.value;
  };
  const state = async () => (await eval_('JSON.stringify(globalThis.__nt ?? null)')) ? JSON.parse(await eval_('JSON.stringify(globalThis.__nt ?? null)')) : null;
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

  const t0 = Date.now();
  const url = `http://127.0.0.1:${VITE_PORT}/nt.html?tar=/gex/nt/${tar.split('/').pop()}&exe=/t/window.exe&args=120&size=800x600`;
  log(`open ${url}`);
  await browser.send('Page.navigate', { url }, page);
  await until((s) => /shown/.test(s.out), 300000, '"shown"');
  check('the program starts and paints', true, `${((Date.now() - t0) / 1000).toFixed(1)} s`);
  await sleep(1500);

  const desc = await eval_('globalThis.__gexCompositor?.describe()', true);
  const win = (desc?.list ?? []).map((l) => /role=1 (\d+)x(\d+)@(-?\d+),(-?\d+).*"(.*)"/.exec(l)).find((m) => m && m[5] === 'gex window');
  check('the compositor has the window', !!win, (desc?.list ?? []).join('; '));
  if (!win) return;
  const [ww, wh, wx, wy] = win.slice(1, 5).map(Number);
  check('at its size', ww === 320 && wh === 240, `${ww}x${wh}`);

  const shot = async (name) => {
    const { data } = await browser.send('Page.captureScreenshot', { format: 'png' }, page);
    const buf = Buffer.from(data, 'base64');
    if (shotDir) {
      mkdirSync(shotDir, { recursive: true });
      writeFileSync(join(shotDir, `${name}.png`), buf);
    }
    return decodePng(buf);
  };
  let img = await shot('shown');
  const px = (x, y) => img.at(wx + x, wy + y);
  check('red top left', near(px(40, 30), [220, 40, 40]), px(40, 30).join(','));
  check('green top right', near(px(280, 30), [40, 200, 60]), px(280, 30).join(','));
  check('blue bottom left', near(px(40, 210), [40, 80, 220]), px(40, 210).join(','));
  check('yellow bottom right', near(px(280, 210), [230, 210, 40]), px(280, 210).join(','));
  check('white centre', near(px(160, 120), [255, 255, 255]), px(160, 120).join(','));

  const [cx, cy] = [250, 190];
  const mouse = (type) => browser.send('Input.dispatchMouseEvent', { type, x: wx + cx, y: wy + cy, button: 'left', clickCount: 1, buttons: type === 'mousePressed' ? 1 : 0 }, page);
  await mouse('mouseMoved');
  await mouse('mousePressed');
  await mouse('mouseReleased');
  const s = await until((s) => /click \d+ \d+/.test(s.out), 30000, 'the click');
  const got = /click (\d+) (\d+)/.exec(s.out);
  check('the click reaches the program where it landed', Math.abs(got[1] - cx) <= 1 && Math.abs(got[2] - cy) <= 1, got[0]);
  await sleep(1000);
  img = await shot('clicked');
  check('and its mark is drawn there', near(px(cx, cy), [0, 0, 0]), px(cx, cy).join(','));
  check('next to yellow', near(px(cx + 10, cy), [230, 210, 40]), px(cx + 10, cy).join(','));

  await browser.send('Input.dispatchKeyEvent', { type: 'keyDown', key: 'Escape', code: 'Escape', windowsVirtualKeyCode: 27 }, page);
  await browser.send('Input.dispatchKeyEvent', { type: 'keyUp', key: 'Escape', code: 'Escape', windowsVirtualKeyCode: 27 }, page);
  const end = await until((s) => s.phase === 'done', 30000, 'the exit');
  check('Escape reaches it and it exits 0', /key 27/.test(end.out) && /bye/.test(end.out) && end.status === 0, `status ${end.status}`);
  if (keep) {
    log('--keep: leaving the browser up; ^C to stop');
    await new Promise(() => {});
  }
}

main()
  .catch((e) => {
    console.log(`FAIL ${e.message}`);
    failed++;
  })
  .finally(() => {
    console.log(failed ? `${failed} failed` : '0 failures');
    stopAll();
    process.exit(failed ? 1 : 0);
  });
