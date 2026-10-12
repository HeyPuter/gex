// The demo desktop in a real browser: the demo build's one test (docs/gexcc.md §9).
//
//   node tests/demo/desktop.mjs [DIST]          (default web/dist-demo)
//
// Serves DIST (COOP/COEP, on 127.0.0.1), opens it in headless Chromium and
// waits for the desktop: both windows up (the compositor has mapped the
// xterm; the launcher is the page's). Prints the page's marks (ms from
// navigation): runtime fetched, rootfs fetched, system up, desktop.
//
// DEMO_STEPS, a JSON list, then drives it (the measurements for §9):
//   {"launch": "ID"}                     the launcher's item ID (download + overlay + run)
//   {"install": "ID"}                    only its overlay, timed
//   {"run": "CMD", "label": "L"}         CMD in pid 1's session, timed in the guest
//                                        ($EPOCHREALTIME); waits for it to end
//   {"windows": N}                       wait for N mapped windows
//   {"hashes": N, "every": MS}           N readouts of the output MS apart with no
//                                        input; "expect": "change" fails if all equal
//   {"cursor": "none|default|image"}     the canvas's CSS cursor after a pointer move
//                                        onto the focused window (no composited one)
//   {"move": [X, Y]}                     the pointer to CSS pixel X, Y
//   {"click": [X, Y]}                    a left click there
//   {"keys": ["Escape", ...]}            named keys (KeyboardEvent.code)
// A run step with "show": true prints the command's output tail even on success.
// Env: CHROME, DEMO_WALL (s, default 300), DEMO_WEBGPU=sw (SwiftShader),
// DEMO_QUERY (appended to the URL), DEMO_PROFILE (a profile dir to keep),
// DEMO_DPR (--force-device-scale-factor), DEMO_CONSOLE (a file for every
// console line), DEMO_CHROME_FLAGS (more browser flags, space-separated).
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { createReadStream, existsSync, statSync, mkdtempSync, rmSync } from 'node:fs';
import { join, extname, resolve, dirname } from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { Cdp, browserWsUrl } from '../../web/test/cdp.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const DIST = resolve(process.argv[2] || join(HERE, '../../web/dist-demo'));
const CHROME = process.env.CHROME || '/usr/bin/chromium';
const WALL = Number(process.env.DEMO_WALL || 300) * 1000;
const STEPS = JSON.parse(process.env.DEMO_STEPS || '[]');
const t0 = Date.now();
const log = (s) => console.log(`[${((Date.now() - t0) / 1000).toFixed(1)}s] ${s}`);

const TYPES = { '.html': 'text/html', '.js': 'text/javascript', '.mjs': 'text/javascript', '.css': 'text/css',
  '.wasm': 'application/wasm', '.json': 'application/json', '.zst': 'application/zstd' };
let served = 0;
const server = createServer((req, res) => {
  let p = join(DIST, decodeURIComponent(new URL(req.url, 'http://x').pathname));
  if (!p.startsWith(DIST)) return res.writeHead(403).end();
  if (existsSync(p) && statSync(p).isDirectory()) p = join(p, 'index.html');
  if (!existsSync(p)) p = join(DIST, 'index.html');
  const size = statSync(p).size;
  served += size;
  res.writeHead(200, { 'Content-Type': TYPES[extname(p)] || 'application/octet-stream', 'Content-Length': size,
    'Cross-Origin-Opener-Policy': 'same-origin', 'Cross-Origin-Embedder-Policy': 'require-corp',
    'Cross-Origin-Resource-Policy': 'same-origin', 'Cache-Control': 'no-cache' });
  createReadStream(p).pipe(res);
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const url = `http://127.0.0.1:${server.address().port}/${process.env.DEMO_QUERY ? `?${process.env.DEMO_QUERY}` : ''}`;
const prof = process.env.DEMO_PROFILE || mkdtempSync(join(tmpdir(), 'gex-desktop-'));
const port = 9400 + Math.floor(Math.random() * 500);
const sw = process.env.DEMO_WEBGPU === 'sw';
const chrome = spawn(CHROME, [
  // DEMO_HEADED=1: not headless mode but the full browser on Ozone's headless
  // platform -- the display compositor a person's browser has, whose
  // screenshots hold the WebGPU canvas (headless mode's do not).
  ...(process.env.DEMO_HEADED ? ['--ozone-platform=headless'] : ['--headless']), '--no-sandbox', '--disable-gpu-sandbox', '--enable-unsafe-webgpu',
  ...(sw ? ['--use-angle=swiftshader-webgl', '--use-webgpu-adapter=swiftshader', '--enable-unsafe-swiftshader']
    : ['--use-angle=vulkan', '--enable-features=Vulkan']),
  ...(process.env.DEMO_DPR ? [`--force-device-scale-factor=${process.env.DEMO_DPR}`] : []),
  ...(process.env.DEMO_CHROME_FLAGS ? process.env.DEMO_CHROME_FLAGS.split(/\s+/).filter(Boolean) : []),
  '--window-size=1280,800', '--enable-logging=stderr', '--v=0',
  `--remote-debugging-port=${port}`, '--remote-allow-origins=*', `--user-data-dir=${prof}`, 'about:blank',
], { stdio: ['ignore', 'ignore', 'pipe'] });
let stderr = '';
chrome.stderr.on('data', (d) => (stderr += d));
const cleanup = () => {
  try { chrome.kill('SIGKILL'); } catch {}
  if (!process.env.DEMO_PROFILE) try { rmSync(prof, { recursive: true, force: true }); } catch {}
};
process.on('exit', cleanup);
const wall = setTimeout(() => {
  log(`FAIL wall of ${WALL / 1000}s`);
  dump();
  process.exit(1);
}, WALL);

let ws = null;
for (let i = 0; i < 100 && !ws; i++) {
  ws = await browserWsUrl(port);
  if (!ws) await new Promise((r) => setTimeout(r, 100));
}
if (!ws) throw new Error('no browser');
const b = await Cdp.connect(ws);
let page = null;
for (let i = 0; i < 50 && !page; i++) {
  const { targetInfos } = await b.send('Target.getTargets');
  page = targetInfos.find((t) => t.type === 'page');
  if (!page) await new Promise((r) => setTimeout(r, 100));
}
if (!page) page = { targetId: (await b.send('Target.createTarget', { url: 'about:blank' })).targetId };
const { sessionId: s } = await b.send('Target.attachToTarget', { targetId: page.targetId, flatten: true });
const consoleLines = [];
b.on('Runtime.consoleAPICalled', (p) => {
  const line = p.args.map((a) => a.value ?? a.description ?? '').join(' ');
  consoleLines.push(line);
  if (/\[desktop\]|error|Error/.test(line) && p.type !== 'debug') log(`console: ${line.slice(0, 300)}`);
});
b.on('Runtime.exceptionThrown', (p) => log(`exception: ${p.exceptionDetails?.exception?.description?.slice(0, 400)}`));
await b.send('Runtime.enable', {}, s);
await b.send('Page.enable', {}, s);
const ev = async (expr) => (await b.send('Runtime.evaluate', { expression: expr, returnByValue: true, awaitPromise: true }, s)).result?.value;
const until = async (expr, what, ms = WALL) => {
  const end = Date.now() + ms;
  for (;;) {
    const v = await ev(expr).catch(() => undefined);
    if (v) return v;
    const err = await ev('globalThis.gexDesktop?.error').catch(() => undefined);
    if (err) throw new Error(`the page failed: ${err}`);
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 200));
  }
};
async function shot(path, after) {
  await new Promise((r) => setTimeout(r, after));
  const { data } = await b.send('Page.captureScreenshot', { format: 'png' }, s);
  const fs = await import('node:fs');
  fs.writeFileSync(path, Buffer.from(data, 'base64'));
  // And the compositor's own output, read back: a headless screenshot may
  // not hold a WebGPU canvas.
  const r = await ev('globalThis.__gexCompositor?.readoutBase64?.()');
  if (r) fs.writeFileSync(path.replace(/\.png$/, '.out.png'), png(r.w, r.h, Buffer.from(r.b64, 'base64')));
  log(`screenshot ${path}${r ? ` and the compositor's ${r.w}x${r.h} output` : ''}`);
}
function png(w, h, rgba) {
  const zlib = globalThis.process.getBuiltinModule('node:zlib');
  const crcT = Array.from({ length: 256 }, (_, n) => { let c = n; for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1; return c >>> 0; });
  const crc = (buf) => { let c = 0xffffffff; for (const x of buf) c = crcT[(c ^ x) & 255] ^ (c >>> 8); return (c ^ 0xffffffff) >>> 0; };
  const chunk = (type, data) => { const l = Buffer.alloc(4); l.writeUInt32BE(data.length); const td = Buffer.concat([Buffer.from(type), data]); const c = Buffer.alloc(4); c.writeUInt32BE(crc(td)); return Buffer.concat([l, td, c]); };
  const raw = Buffer.alloc((w * 4 + 1) * h);
  for (let y = 0; y < h; y++) rgba.copy(raw, y * (w * 4 + 1) + 1, y * w * 4, (y + 1) * w * 4);
  const ihdr = Buffer.alloc(13); ihdr.writeUInt32BE(w, 0); ihdr.writeUInt32BE(h, 4); ihdr[8] = 8; ihdr[9] = 6;
  return Buffer.concat([Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), chunk('IHDR', ihdr), chunk('IDAT', zlib.deflateSync(raw)), chunk('IEND', Buffer.alloc(0))]);
}
/** A character's `KeyboardEvent.code`, and whether it needs Shift (US layout). */
function keyCode(ch) {
  if (/[a-z]/.test(ch)) return [`Key${ch.toUpperCase()}`, false];
  if (/[A-Z]/.test(ch)) return [`Key${ch}`, true];
  if (/[0-9]/.test(ch)) return [`Digit${ch}`, false];
  const plain = { ' ': 'Space', '-': 'Minus', '/': 'Slash', '.': 'Period', '=': 'Equal', ';': 'Semicolon',
    ',': 'Comma', "'": 'Quote', '`': 'Backquote', '[': 'BracketLeft', ']': 'BracketRight', '\\': 'Backslash' };
  if (plain[ch]) return [plain[ch], false];
  const shifted = { '|': 'Backslash', '_': 'Minus', '+': 'Equal', ':': 'Semicolon', '"': 'Quote', '~': 'Backquote',
    '!': 'Digit1', '@': 'Digit2', '#': 'Digit3', '$': 'Digit4', '%': 'Digit5', '^': 'Digit6', '&': 'Digit7',
    '*': 'Digit8', '(': 'Digit9', ')': 'Digit0', '<': 'Comma', '>': 'Period', '?': 'Slash', '{': 'BracketLeft', '}': 'BracketRight' };
  return [shifted[ch] ?? '', !!shifted[ch]];
}
/** The first window's centre and an empty spot of desktop, in CSS pixels. */
async function places() {
  return ev(`(() => {
    const c = document.getElementById('out').getBoundingClientRect();
    const k = c.width / document.getElementById('out').width || 1;
    return globalThis.__gexCompositor.describe().then((d) => {
      const m = /role=1 (\\d+)x(\\d+)@(-?\\d+),(-?\\d+)/.exec(d.list.join(' '));
      if (!m) return null;
      const [w, h, x, y] = m.slice(1).map(Number);
      return { win: [c.left + (x + w / 2) * k, c.top + (y + h / 2) * k],
        desk: [c.right - 40, c.bottom - 40], k };
    });
  })()`);
}
/** The page's screenshot, hashed: what the browser displays, where its
 *  screenshots can see a worker's WebGPU canvas (headless ones cannot). */
async function screenHash() {
  const { data } = await b.send('Page.captureScreenshot', { format: 'png' }, s);
  let h = 2166136261;
  for (let i = 0; i < data.length; i++) h = Math.imul(h ^ data.charCodeAt(i), 16777619) >>> 0;
  return h;
}
/** What the page's canvas holds now: the placeholder's last frame, drawn
 *  into a 2D canvas on the page and hashed. A canvas handed to a worker shows
 *  what the worker last committed; this reads that, not the worker's scene. */
async function canvasHash() {
  return ev(`(() => {
    const c = document.getElementById('out');
    const t = new OffscreenCanvas(c.width, c.height);
    const g = t.getContext('2d', { willReadFrequently: true });
    g.drawImage(c, 0, 0);
    const d = g.getImageData(0, 0, c.width, c.height).data;
    let h = 2166136261, lit = 0;
    for (let i = 0; i < d.length; i += 4) {
      h = Math.imul(h ^ d[i] ^ (d[i + 1] << 8) ^ (d[i + 2] << 16), 16777619) >>> 0;
      if (d[i] + d[i + 1] + d[i + 2] > 600) lit++;
    }
    return h + ':' + lit;
  })()`);
}
async function legs() {
  const p = await places();
  if (!p) throw new Error('no window to point at');
  const cursorAt = async ([x, y], want) => {
    await b.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x, y }, s);
    let css = '';
    for (let i = 0; i < 30; i++) {
      await new Promise((r) => setTimeout(r, 150));
      css = (await ev("getComputedStyle(document.getElementById('out')).cursor")) || '';
      if (want.test(css)) break;
    }
    return css;
  };
  const overWin = await cursorAt(p.win, /url\(|image-set/);
  const overDesk = await cursorAt(p.desk, /^default$/);
  const okCursor = /url\(|image-set/.test(overWin) && overDesk === 'default';
  if (!okCursor) failed = true;
  log(`${okCursor ? 'ok' : 'FAIL'} cursor leg: over the xterm ${JSON.stringify(overWin.slice(0, 60))}, over the desktop ${JSON.stringify(overDesk)}`);
  // Back over the xterm, focused, and a loop that prints with no input.
  await b.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: p.win[0], y: p.win[1] }, s);
  await b.send('Input.dispatchMouseEvent', { type: 'mousePressed', x: p.win[0], y: p.win[1], button: 'left', clickCount: 1 }, s);
  await b.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x: p.win[0], y: p.win[1], button: 'left', clickCount: 1 }, s);
  // The shell may still be starting: wait for its prompt to draw.
  let prev = await canvasHash();
  for (let i = 0; i < 20; i++) {
    await new Promise((r) => setTimeout(r, 500));
    const h = await canvasHash();
    if (h === prev) break;
    prev = h;
  }
  for (const ch of 'for i in 1 2 3 4 5; do sleep 1; echo tick$i; done\n') {
    if (ch === '\n') {
      const k = { key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13 };
      await b.send('Input.dispatchKeyEvent', { type: 'keyDown', ...k, text: '\r' }, s);
      await b.send('Input.dispatchKeyEvent', { type: 'keyUp', ...k }, s);
      continue;
    }
    const [code, shift] = keyCode(ch);
    const m = shift ? 8 : 0;
    await b.send('Input.dispatchKeyEvent', { type: 'keyDown', key: ch, text: ch, code, modifiers: m }, s);
    await b.send('Input.dispatchKeyEvent', { type: 'keyUp', key: ch, code, modifiers: m }, s);
  }
  await new Promise((r) => setTimeout(r, 1500));
  const d0 = await ev('globalThis.__gexCompositor.describe()');
  // What the page's canvas holds (its last committed frame, drawn into a 2D
  // canvas), not a screenshot: headless Chromium's screenshots never contain
  // a WebGPU canvas a worker draws, so they cannot tell shown from not.
  const held = [await canvasHash()];
  const shown = [await screenHash()];
  for (let i = 0; i < 4; i++) {
    await new Promise((r) => setTimeout(r, 1000));
    held.push(await canvasHash());
    shown.push(await screenHash());
  }
  const shots = shown.slice(1).filter((h, i) => h !== shown[i]).length;
  const d1 = await ev('globalThis.__gexCompositor.describe()');
  const changes = held.slice(1).filter((h, i) => h !== held[i]).length;
  const okShown = changes >= 3;
  if (!okShown) failed = true;
  log(`${okShown ? 'ok' : 'FAIL'} presentation leg: the page's canvas changed ${changes} times in 4 s of the xterm printing with no input`
    + ` (compositor: ${d1.frames - d0.frames} frames drawn in ${d1.turns - d0.turns} turns; screenshots changed ${shots} times)`);
}
function dump() {
  ev('globalThis.gexTerminalText?.()').then((t) => {
    console.log('--- console (pid 1) ---');
    console.log((t || '').trim().split('\n').slice(-40).join('\n'));
  }).catch(() => {});
}

let failed = false;
try {
  log(`open ${url}`);
  await b.send('Page.navigate', { url }, s);
  await until('globalThis.gexDesktop?.ready', 'the desktop');
  const marks = await ev('globalThis.gexDesktop.marks');
  const rootfs = await ev('globalThis.gexDesktop.rootfs');
  const desc = await ev('globalThis.__gexCompositor?.describe?.()');
  const launcherShown = await ev("!document.getElementById('launcher').classList.contains('hidden')");
  log(`ok desktop: ${JSON.stringify(marks)}; served ${(served / 1048576).toFixed(2)} MiB; ` +
    `rootfs ${JSON.stringify(rootfs)}; launcher ${launcherShown ? 'open' : 'MISSING'}`);
  if (desc) log(`compositor: ${JSON.stringify(desc).slice(0, 400)}`);
  if (!launcherShown) throw new Error('the launcher window is not open');
  if (process.env.DEMO_SHOT) await shot(process.env.DEMO_SHOT, 1500);
  // The legs every run has (DEMO_LEGS=0 skips them): the xterm's cursor is
  // the page's CSS cursor and none is composited; what the xterm prints with
  // no input reaches the screen (the page's own screenshot, not a readback).
  if (process.env.DEMO_LEGS !== '0') await legs();
  let n = 0;
  let lastHash = null;
  for (const step of STEPS) {
    n++;
    if (step.windows) {
      try {
        await until(`(globalThis.__gexCompositor?.counters?.().windows ?? 0) >= ${step.windows}`, `${step.windows} windows`,
          (step.wall ?? WALL / 1000) * 1000);
        log(`ok ${step.windows} windows`);
      } catch (e) {
        failed = true;
        log(`FAIL ${e.message}`);
        if (!step.soft) throw e;
      }
    } else if (step.hashes) {
      // Counted with the last readout of the step before, so a change that
      // lands before this step's first readout still counts. "screen": the
      // page's own screenshot (what is presented), not the compositor's readout.
      const seen = lastHash === null ? [] : [lastHash];
      for (let i = 0; i < step.hashes; i++) {
        await new Promise((r) => setTimeout(r, step.every ?? 1000));
        if (step.screen) {
          const { data } = await b.send('Page.captureScreenshot', { format: 'png', clip: { x: 0, y: 40, width: 600, height: 400, scale: 1 } }, s);
          let h = 2166136261;
          for (let k = 0; k < data.length; k++) h = ((h ^ data.charCodeAt(k)) * 16777619) >>> 0;
          seen.push(h);
        } else {
          const d = await ev('globalThis.__gexCompositor?.describe?.()');
          seen.push(d?.hash ?? 0);
        }
      }
      lastHash = seen[seen.length - 1];
      const distinct = new Set(seen).size;
      const ok = step.expect !== 'change' || distinct > 1;
      if (!ok) failed = true;
      log(`${ok ? 'ok' : 'FAIL'} ${step.label ?? 'hashes'}: ${distinct} distinct of ${seen.length} readouts ${step.every ?? 1000} ms apart`);
    } else if (step.click) {
      const [x, y] = step.click;
      await b.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x, y }, s);
      await b.send('Input.dispatchMouseEvent', { type: 'mousePressed', x, y, button: 'left', clickCount: 1 }, s);
      await b.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x, y, button: 'left', clickCount: 1 }, s);
      await new Promise((r) => setTimeout(r, 300));
      log(`clicked ${step.click}`);
    } else if (step.move) {
      await b.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: step.move[0], y: step.move[1] }, s);
      await new Promise((r) => setTimeout(r, 300));
      log(`moved to ${step.move}`);
    } else if (step.cursor) {
      await new Promise((r) => setTimeout(r, step.after ?? 1500));
      const css = (await ev("getComputedStyle(document.getElementById('out')).cursor")) || '';
      const kind = css.startsWith('url(') || css.includes('image-set') ? 'image' : css;
      const ok = kind === step.cursor;
      if (!ok) failed = true;
      log(`${ok ? 'ok' : 'FAIL'} cursor ${JSON.stringify(css.slice(0, 80))} (want ${step.cursor})`);
    } else if (step.keys) {
      await ev("document.getElementById('out').focus()");
      for (const code of step.keys) {
        const key = code === 'Escape' ? 'Escape' : code === 'Enter' ? 'Enter' : code;
        const vk = { Escape: 27, Enter: 13 }[code] ?? 0;
        await b.send('Input.dispatchKeyEvent', { type: 'keyDown', key, code, windowsVirtualKeyCode: vk, ...(code === 'Enter' ? { text: '\r' } : {}) }, s);
        await b.send('Input.dispatchKeyEvent', { type: 'keyUp', key, code, windowsVirtualKeyCode: vk }, s);
      }
      log(`keys ${step.keys.join(' ')}`);
    } else if (step.shot) {
      await shot(step.shot, step.after ?? 1500);
    } else if (step.type) {
      // Keys as a person types them, into whatever the compositor focuses.
      await ev("document.getElementById('out').focus()");
      for (const ch of step.type) {
        if (ch === '\n') {
          const k = { key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13 };
          await b.send('Input.dispatchKeyEvent', { type: 'keyDown', ...k, text: '\r' }, s);
          await b.send('Input.dispatchKeyEvent', { type: 'keyUp', ...k }, s);
        } else {
          const [code, shift] = keyCode(ch);
          const sh = { key: 'Shift', code: 'ShiftLeft', windowsVirtualKeyCode: 16 };
          if (shift) await b.send('Input.dispatchKeyEvent', { type: 'keyDown', ...sh, modifiers: 8 }, s);
          const m = shift ? 8 : 0;
          await b.send('Input.dispatchKeyEvent', { type: 'keyDown', key: ch, text: ch, code, modifiers: m }, s);
          await b.send('Input.dispatchKeyEvent', { type: 'keyUp', key: ch, code, modifiers: m }, s);
          if (shift) await b.send('Input.dispatchKeyEvent', { type: 'keyUp', ...sh }, s);
        }
      }
      log(`typed ${JSON.stringify(step.type)}`);
    } else if (step.install || step.launch) {
      const id = step.install || step.launch;
      const a = Date.now();
      if (step.install) await ev(`globalThis.gexDesktop.install(${JSON.stringify(id)})`);
      else await ev(`globalThis.gexDesktop.launch(${JSON.stringify(id)})`);
      const r = await ev(`globalThis.gexDesktop.installs?.[${JSON.stringify(id)}]`);
      log(`ok ${step.install ? 'install' : 'launch'} ${id}: ${Date.now() - a} ms; overlay ${JSON.stringify(r)}`);
    } else if (step.run) {
      const tag = `S${n}x${Math.floor(Math.random() * 1e6)}`;
      const cmd = `s=$EPOCHREALTIME; { ${step.run} ; } >/tmp/${tag}.out 2>&1; r=$?; e=$EPOCHREALTIME; ` +
        `printf "\\r\\n"; echo "${tag.toLowerCase()} rc=$r $(( (\${e/./}-\${s/./})/1000 ))ms"; tail -${process.env.DEMO_TAIL || 5} /tmp/${tag}.out`;
      await ev(`globalThis.gexDesktop.run(${JSON.stringify(cmd)})`);
      const re = new RegExp(`${tag.toLowerCase()} rc=(\\d+) (\\d+)ms`, 'm');
      const got = await until(`(() => { const m = /${re.source}/m.exec(globalThis.gexTerminalText()); return m && [m[1], m[2]]; })()`,
        `"${step.label ?? step.run}"`, (step.wall ?? 600) * 1000);
      log(`${got[0] === '0' ? 'ok' : 'FAIL'} run ${step.label ?? step.run}: rc=${got[0]} ${got[1]} ms (guest clock)`);
      if (got[0] !== '0' || step.show) {
        if (got[0] !== '0') failed = true;
        // The command's last lines, which the session prints after the result.
        await new Promise((r) => setTimeout(r, 500));
        const t = (await ev('globalThis.gexTerminalText()')) || '';
        const at = t.search(re);
        for (const l of t.slice(at).split('\n').slice(1, Number(process.env.DEMO_TAIL || 6))) if (l.trim()) log(`  | ${l}`);
      }
    }
  }
} catch (e) {
  failed = true;
  log(`FAIL ${e.message}`);
  await new Promise((r) => { dump(); setTimeout(r, 500); });
}
clearTimeout(wall);
// DEMO_CONSOLE=PATH: every console line the page logged, for a trace.
if (process.env.DEMO_CONSOLE) (await import('node:fs')).writeFileSync(process.env.DEMO_CONSOLE, consoleLines.join('\n'));
const errs = stderr.split('\n').filter((l) => /Uncaught|RuntimeError|\[gex-console-error\]/.test(l));
for (const l of errs.slice(0, 10)) log(`stderr: ${l.slice(0, 300)}`);
b.close();
server.close();
cleanup();
process.exit(failed ? 1 : 0);
