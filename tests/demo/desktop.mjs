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
//   {"eval": "EXPR"}                     EXPR in the page, its value printed
//   {"gl": true}                         the glxgears legs: from the xterm, then the launcher
//   {"glcycles": N}                      glxgears from the launcher, killed, N times: GPU
//                                        memory (nvidia-smi) must not grow per client, nor
//                                        while one animates ("hold" s, 30; "slack" MiB, 64)
//   {"resize": "TITLE", "maximize": true}  drag the window's corner; it must redraw bigger;
//                                        then a title double-click maximizes and restores
//   {"fits": "ID"}                       the launcher's item ID opens inside the output, at a
//                                        terminal's size in CSS pixels at any dpr
//   {"close": "NAME"}                   end program NAME; its window must go ("title")
//   {"animates": "TITLE"}                a window titled TITLE appears and its own pixels
//                                        change ("samples", "every" ms, "min" changes)
//   {"truth": true, "rounds": N}         type into the xterm, then compare the X server's
//                                        copy of it (xwd) with the compositor's output
//                                        (needs the python and xeyes overlays)
//   {"pools": true, "seconds": N}        the xterm prints a line a second for N s; the
//                                        X server's live pools must not grow
// A run step with "show": true prints the command's output tail even on success.
// Env: CHROME, DEMO_WALL (s, default 300), DEMO_WEBGPU=sw (SwiftShader),
// DEMO_QUERY (appended to the URL), DEMO_SIGTERM=hard (a program that ignores
// SIGTERM fails, rather than being a known failure), DEMO_PROFILE (a profile dir to keep),
// DEMO_DPR (--force-device-scale-factor), DEMO_CONSOLE (a file for every
// console line), DEMO_CHROME_FLAGS (more browser flags, space-separated),
// DEMO_STDERR (a file for the browser's whole stderr), DEMO_COUNT=1 (print
// the X server's ?hostxcount lines), DEMO_GL=0 (skip the
// glxgears legs), DEMO_FOOT=0 (skip the foot leg), DEMO_VRAM=0 (skip the GPU memory leg),
// DEMO_GPU_TOTAL=1 (measure it by the card's total when nvidia-smi lists no process of ours).
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
// Whatever ends the run says so in the log, as a FAIL line: a leg that dies
// with only a stack trace on stderr reads, to a harness that greps the log,
// like a run that printed nothing at all.
for (const ev of ['uncaughtException', 'unhandledRejection']) {
  process.on(ev, (e) => {
    log(`FAIL ${ev}: ${e?.stack || e}`);
    process.exit(1);
  });
}

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
// hostxcount=1 always: the pool leg reads the X server's live pools from it.
const query = ['hostxcount=1', process.env.DEMO_QUERY].filter(Boolean).join('&');
const url = `http://127.0.0.1:${server.address().port}/?${query}`;
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
let closing = false;
chrome.on('exit', (code, sig) => {
  if (closing) return;
  log(`FAIL the browser exited (code ${code}, signal ${sig}); its last words:`);
  for (const l of stderr.split('\n').filter(Boolean).slice(-15)) log(`  ${l.slice(0, 300)}`);
  process.exit(1);
});
const cleanup = () => {
  closing = true;
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
if (!ws) throw new Error(`no browser on port ${port} after 10 s`);
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
/** The X server's pools made and not freed, from its last `?hostxcount` line. */
let xPools = null;
b.on('Runtime.consoleAPICalled', (p) => {
  const line = p.args.map((a) => a.value ?? a.description ?? '').join(' ');
  consoleLines.push(line);
  const pools = /\[hostx-count\].* pools=(\d+)/.exec(line);
  if (pools) {
    xPools = Number(pools[1]);
    if (process.env.DEMO_COUNT) log(`console: ${line.slice(0, 300)}`);
    return;
  }
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
/** One key into the focused window, as the page gets it from a person. */
async function typeKey(ch) {
  if (ch === '\n') {
    const k = { key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13 };
    await b.send('Input.dispatchKeyEvent', { type: 'keyDown', ...k, text: '\r' }, s);
    await b.send('Input.dispatchKeyEvent', { type: 'keyUp', ...k }, s);
    return;
  }
  const [code, shift] = keyCode(ch);
  const m = shift ? 8 : 0;
  await b.send('Input.dispatchKeyEvent', { type: 'keyDown', key: ch, text: ch, code, modifiers: m }, s);
  await b.send('Input.dispatchKeyEvent', { type: 'keyUp', key: ch, code, modifiers: m }, s);
}
async function click(x, y) {
  await b.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x, y }, s);
  await b.send('Input.dispatchMouseEvent', { type: 'mousePressed', x, y, button: 'left', clickCount: 1 }, s);
  await b.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x, y, button: 'left', clickCount: 1 }, s);
}
/** A command in pid 1's session (not the xterm's); its output's lines. */
async function guestRun(cmd, wall = 120000) {
  const tag = `g${Math.floor(Math.random() * 1e9)}`;
  await ev(`globalThis.gexDesktop.run(${JSON.stringify(`{ ${cmd} ; } >/tmp/${tag}.out 2>&1; echo "${tag} rc=$?"; cat /tmp/${tag}.out; echo "${tag} end"`)})`);
  await until(`globalThis.gexTerminalText().includes(${JSON.stringify(`${tag} end`)})`, cmd.slice(0, 40), wall);
  const t = (await ev('globalThis.gexTerminalText()')) || '';
  const body = t.slice(t.search(new RegExp(`${tag} rc=`)));
  const rc = Number(/rc=(\d+)/.exec(body)[1]);
  return { rc, lines: body.split('\n').slice(1).filter((l) => !l.includes(`${tag} end`)) };
}
const SIG_COLS = 16, SIG_ROWS = 8, SIG_EDGE = 2;
/** The same signature as tests/demo/guest/xsig.py, of what the compositor
 *  composed where the window is. */
async function composedSig(x, y, w, h) {
  // Computed in the page, like windowHash: only the signature crosses CDP.
  for (let t = 0; t < 3; t++) {
    const sig = await ev(`globalThis.__gexCompositor.readout().then((r) => {
      if (!r) return null;
      const [x, y, w, h, COLS, ROWS] = [${x}, ${y}, ${w}, ${h}, ${SIG_COLS}, ${SIG_ROWS}];
      const cells = new Array(COLS * ROWS).fill(0);
      let exact = 2166136261;
      for (let j = 0; j < h; j++) {
        const cy = Math.floor(j * ROWS / h) * COLS;
        for (let i = 0; i < w; i++) {
          const o = ((y + j) * r.w + (x + i)) * 4;
          const dark = Math.max(r.px[o], r.px[o + 1], r.px[o + 2]) < 128 ? 1 : 0;
          if (dark) cells[cy + Math.floor(i * COLS / w)]++;
          exact = Math.imul(exact ^ dark, 16777619) >>> 0;
        }
      }
      return { cells, exact };
    })`);
    if (sig) {
      const cells = sig.cells;
      cells.exact = sig.exact;
      return cells;
    }
  }
  const d = await ev('globalThis.__gexCompositor.describe()');
  throw new Error(`the compositor gave no readout in three tries; its log: ${JSON.stringify(d?.log ?? d)}`);
}
/**
 * Bug 3 as a leg: what the xterm drew is what the page shows. Type into the
 * focused xterm at a person's pace -- each key a frame, so Xwayland draws into
 * the buffers it already has rather than making new ones -- then leave it
 * alone, and compare the X server's own copy of the window (xwd, in the guest)
 * with the compositor's output where the window is.
 */
async function truth(step) {
  const script = (await import('node:fs')).readFileSync(new URL('./guest/xsig.py', import.meta.url)).toString('base64');
  const put = await guestRun(`echo ${script} | base64 -d > /tmp/xsig.py`);
  if (put.rc) throw new Error('could not write /tmp/xsig.py');
  const p = await places();
  await click(...p.win);
  await new Promise((r) => setTimeout(r, 1000));
  let bad = 0;
  const rounds = step.rounds ?? 4;
  for (let n = 0; n < rounds; n++) {
    for (const ch of `echo round ${n} the quick brown fox ${'x'.repeat(n * 3)}\n`) {
      await typeKey(ch);
      await new Promise((r) => setTimeout(r, step.pace ?? 60));
    }
    // The pointer across the window's edge and back: the X server makes a
    // cursor pool for each shape it shows, between the xterm's frames.
    for (let i = 0; i < (step.wiggle ?? 3); i++) {
      await b.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: p.desk[0], y: p.desk[1] }, s);
      await new Promise((r) => setTimeout(r, 150));
      await b.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: p.win[0], y: p.win[1] }, s);
      await new Promise((r) => setTimeout(r, 150));
      await typeKey(String.fromCharCode(97 + i));
    }
    await new Promise((r) => setTimeout(r, step.settle ?? 2000));
    const d = await ev('globalThis.__gexCompositor.describe()');
    const line = d.list.find((l) => / role=1 /.test(l));
    const [, w, h, x, y] = /(\d+)x(\d+)@(-?\d+),(-?\d+)/.exec(line).map(Number);
    const title = d.titles[0];
    const got = await guestRun(`python3.13 /tmp/xsig.py ${JSON.stringify(title)}`);
    const size = got.lines.find((l) => l.startsWith('xsize'));
    if (got.rc || !size) throw new Error(`xsig: rc=${got.rc} ${got.lines.slice(0, 4).join(' | ')}`);
    const drawn = [];
    for (let r = 0; r < SIG_ROWS; r++) {
      const l = got.lines.find((v) => v.startsWith(`xs${r} `));
      drawn.push(...l.slice(l.indexOf(' ') + 1).split(',').map(Number));
    }
    // The X window inside the surface: Xwayland's buffer carries the
    // window's border, which xwd leaves out -- so the best of a few offsets.
    const [xw, xh, xexact] = size.split(' ').slice(1).map(Number);
    let best = null;
    for (const [dx, dy] of [[0, 0], [1, 1], [-1, -1], [1, 0], [0, 1], [2, 2]]) {
      const sig = await composedSig(x + dx + SIG_EDGE, y + dy + SIG_EDGE, xw - 2 * SIG_EDGE, xh - 2 * SIG_EDGE);
      const d = drawn.reduce((a, v, i) => a + Math.abs(v - sig[i]), 0) + (sig.exact === xexact ? 0 : 0.5);
      if (!best || d < best.d) best = { d, sig, at: `${dx},${dy}` };
      if (!d) break;
    }
    const shown = best.sig;
    const diff = best.d;
    const cells = drawn.filter((v, i) => v !== shown[i]).length;
    // Equal counts and an equal bitmap: a cursor block one character along
    // stays in its cell, and only the bitmap tells.
    const ok = cells === 0 && shown.exact === xexact;
    if (!ok) bad++;
    if (!ok && process.env.DEMO_SIG) {
      for (let r = 0; r < SIG_ROWS; r++) {
        log(`    X ${drawn.slice(r * SIG_COLS, (r + 1) * SIG_COLS).join(',')}`);
        log(`    C ${shown.slice(r * SIG_COLS, (r + 1) * SIG_COLS).join(',')}`);
      }
    }
    log(`  round ${n}: X window ${size.split(' ').slice(1).join('x')} vs composed ${w}x${h}@${x},${y}: `
      + `${ok ? 'same' : `${cells} of ${drawn.length} cells differ (${Math.floor(diff)} dark pixels), bitmaps ${shown.exact === xexact ? 'equal' : 'differ'}`} at +${best.at} (serial ${/serial=(\d+)/.exec(line)[1]})`);
  }
  if (bad) failed = true;
  log(`${bad ? 'FAIL' : 'ok'} xterm shows what it drew: ${rounds - bad} of ${rounds} rounds`);
}
/** A hash of what the compositor shows where the window titled `title` is. */
async function windowHash(title) {
  const d = await ev('globalThis.__gexCompositor.describe()');
  const line = d.list.find((l) => / role=1 /.test(l) && l.endsWith(`"${title}"`));
  if (!line) return null;
  const [, w, h, x, y] = /(\d+)x(\d+)@(-?\d+),(-?\d+)/.exec(line).map(Number);
  // Hashed in the page: only the number crosses CDP. Returning the whole
  // output (12 MiB of base64 at dpr 2) for every sample is what grew the
  // browser's GPU memory by ~500 MiB per GL client in the glcycles leg.
  for (let t = 0; t < 3; t++) {
    const hash = await ev(`globalThis.__gexCompositor.readout().then((r) => {
      if (!r) return null;
      let hash = 2166136261;
      for (let j = Math.max(0, ${y}); j < Math.min(r.h, ${y + h}); j++) {
        for (let i = Math.max(0, ${x}); i < Math.min(r.w, ${x + w}); i++) {
          const o = (j * r.w + i) * 4;
          hash = Math.imul(hash ^ (r.px[o] | (r.px[o + 1] << 8) | (r.px[o + 2] << 16)), 16777619) >>> 0;
        }
      }
      return hash;
    })`);
    if (hash !== null && hash !== undefined) return hash;
  }
  const dd = await ev('globalThis.__gexCompositor.describe()');
  throw new Error(`the compositor gave no readout in three tries; its log: ${JSON.stringify(dd?.log ?? dd)}`);
}
/**
 * A window titled `animates` appears within `wall` seconds and its own pixels
 * (not the rest of the screen: a terminal printing frame rates changes too)
 * change from one readout to the next.
 */
async function animates(step) {
  const title = step.animates;
  const end = Date.now() + (step.wall ?? 30) * 1000;
  let first = null;
  while (Date.now() < end && (first = await windowHash(title)) === null) await new Promise((r) => setTimeout(r, 500));
  if (first === null) {
    failed = true;
    const d = await ev('globalThis.__gexCompositor.describe()');
    const ps = await guestRun('ps ax -o pid,stat,tty,comm; cat /tmp/gears.rc 2>/dev/null').catch(() => ({ lines: [] }));
    log(`FAIL ${step.label ?? title}: no window titled ${JSON.stringify(title)} in ${step.wall ?? 30} s;`
      + ` windows ${JSON.stringify(d.list)}; processes ${JSON.stringify(ps.lines.map((l) => l.trim()).filter(Boolean))}`);
    return;
  }
  const seen = [first];
  for (let i = 0; i < (step.samples ?? 5); i++) {
    await new Promise((r) => setTimeout(r, step.every ?? 600));
    seen.push(await windowHash(title));
  }
  const changes = seen.slice(1).filter((h, i) => h !== seen[i]).length;
  const ok = changes >= (step.min ?? 3);
  if (!ok) failed = true;
  const d = await ev('globalThis.__gexCompositor.describe()');
  log(`${ok ? 'ok' : 'FAIL'} ${step.label ?? `${title} animates`}: its window's pixels changed ${changes} times in ${seen.length - 1} readouts`
    + `${ok ? '' : `; compositor log: ${JSON.stringify(d.log.slice(-6))}`}`);
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
  // What the xterm drew is what the page shows (bug 3): xwd and python come
  // as overlays, the way a person adds them from the launcher.
  await new Promise((r) => setTimeout(r, 3000));
  for (const id of ['python', 'xeyes']) await ev(`globalThis.gexDesktop.install(${JSON.stringify(id)})`);
  await truth({ rounds: 3 });
  await poolLeg({});
  if (process.env.DEMO_GL !== '0') await glLeg();
  // GPU memory: back near where it was after each GL client, and flat while
  // one animates (DEMO_VRAM=0 skips it).
  if (process.env.DEMO_GL !== '0' && process.env.DEMO_VRAM !== '0') await glCycles({ glcycles: 4 });
  if (process.env.DEMO_FOOT !== '0') {
    await fitsLeg({ fits: 'foot', keep: true });
    await resizeLeg({ resize: 'foot', maximize: true });
    await closeProgram('foot');
  }
  if (process.env.DEMO_RESIZE !== '0') await resizeLeg({ resize: 'bash', label: 'the xterm' });
}
/**
 * glxgears from the xterm, typed, and then from the launcher: both are the
 * same path (the launcher runs the command as a shell would), and each must
 * show a window whose own pixels move -- frame rates printed in the xterm do
 * not count. The second is also the second GL client to take the same X
 * window id, which is what once left it with no window at all.
 */
async function glLeg() {
  await ev(`globalThis.gexDesktop.install('gears')`);
  const p = await places();
  await click(...p.win);
  await new Promise((r) => setTimeout(r, 500));
  // Its exit status, for a run where no window comes: typed as a shell runs it.
  for (const ch of '\nrm -f /tmp/gears.rc; glxgears; echo $? > /tmp/gears.rc\n') await typeKey(ch);
  await animates({ animates: 'glxgears', label: 'glxgears from the xterm animates', wall: 60 });
  await guestRun('kill $(pidof glxgears); sleep 2');
  await glForgotten('the xterm\'s glxgears');
  await ev(`globalThis.gexDesktop.launch('gears')`);
  await animates({ animates: 'glxgears', label: 'glxgears from the launcher, after it, animates', wall: 60 });
  await guestRun('kill $(pidof glxgears); sleep 1');
  await glForgotten('the launcher\'s glxgears');
}
/** Where each of the page's Workers is, for a Worker that stopped answering:
 *  paused over CDP, its innermost frames. One that will not pause in 3 s is
 *  blocked (Atomics.wait), not spinning. */
async function workerStacks(match = /compositor/) {
  const attached = [];
  const onAttach = (p) => {
    if (p.targetInfo?.type === 'worker' && match.test(p.targetInfo.url)) attached.push(p);
  };
  b.on('Target.attachedToTarget', onAttach);
  await b.send('Target.setAutoAttach', { autoAttach: true, waitForDebuggerOnStart: false, flatten: true }, s);
  await new Promise((r) => setTimeout(r, 1000));
  for (const a of attached) {
    const sid = a.sessionId;
    const paused = new Promise((resolve) => {
      b.on('Debugger.paused', (p, from) => { if (from === sid) resolve(p); });
      setTimeout(() => resolve(null), 3000);
    });
    try {
      await b.send('Debugger.enable', {}, sid);
      await b.send('Debugger.pause', {}, sid);
    } catch (e) {
      log(`  ${a.targetInfo.url.split('/').pop()}: ${e.message}`);
      continue;
    }
    const p = await paused;
    log(`  ${a.targetInfo.url.split('/').pop()}: ${p ? 'paused at' : 'did not pause in 3 s (blocked, not spinning)'}`);
    for (const f of (p?.callFrames ?? []).slice(0, 24)) {
      log(`    ${f.functionName || '(anonymous)'} ${f.url.split('/').pop()}:${f.location.lineNumber}`);
    }
    try {
      await b.send('Debugger.resume', {}, sid);
    } catch { /* gone */ }
  }
}
/**
 * A Wayland program started from the launcher opens at a size that fits the
 * output, drawn at the page's density. foot used to open four screens wide at
 * dpr 2: every new window was told the whole output, and no surface was ever
 * said to be on the output, so foot never learned its scale. Fits: its frame
 * is inside the output. Sane: its size in CSS pixels is a terminal's, not
 * double (or half) one -- the same at every dpr.
 */
async function fitsLeg(step) {
  const id = step.fits;
  await ev(`globalThis.gexDesktop.launch(${JSON.stringify(id)})`);
  const title = step.title ?? id;
  let d = null, line = null;
  const end = Date.now() + (step.wall ?? 60) * 1000;
  while (Date.now() < end) {
    d = await ev('globalThis.__gexCompositor.describe()');
    line = d?.list.find((l) => / role=1 /.test(l) && l.endsWith(`"${title}"`));
    if (line) break;
    await new Promise((r) => setTimeout(r, 500));
  }
  if (!line) {
    failed = true;
    log(`FAIL ${id} fits: no window titled ${JSON.stringify(title)}; windows ${JSON.stringify(d?.list)}`);
    return;
  }
  // Settled: a client may resize once it has its first configure.
  await new Promise((r) => setTimeout(r, 2000));
  d = await ev('globalThis.__gexCompositor.describe()');
  line = d.list.find((l) => / role=1 /.test(l) && l.endsWith(`"${title}"`)) ?? line;
  const [, w, h, x, y] = /(\d+)x(\d+)@(-?\d+),(-?\d+)/.exec(line).map(Number);
  const [outW, outH] = d.output.split('x').map(Number);
  const dpr = await ev('devicePixelRatio');
  const inside = x >= 0 && y >= 0 && x + w <= outW && y + h <= outH;
  const css = [Math.round(w / dpr), Math.round(h / dpr)];
  // foot's default is 700x500 surface units; a terminal of 80x24 cells of a
  // normal font is 500-1000 by 300-700.
  const sane = css[0] >= 300 && css[0] <= 1100 && css[1] >= 200 && css[1] <= 800;
  const ok = inside && sane;
  if (!ok) failed = true;
  log(`${ok ? 'ok' : 'FAIL'} ${id} fits: ${w}x${h} at ${x},${y} in a ${outW}x${outH} output (dpr ${dpr}), ${css[0]}x${css[1]} CSS pixels`
    + `${inside ? '' : '; past the output'}${sane ? '' : '; not a terminal\'s size'}`);
  if (!step.keep) await closeProgram(step.process ?? id, title);
}
/**
 * End a program and wait for its window to go. `pidof` alone missed foot (its
 * window stayed, and covered the xterm's corner in the resize leg that
 * followed), so the process table is searched by name as well.
 */
async function closeProgram(name, title = name) {
  const pids = `$(pidof ${name}) $(ps -eo pid=,comm= | awk '$2 == "${name}" {print $1}')`;
  const gone = async (tries) => {
    for (let i = 0; i < tries; i++) {
      const d = await ev('globalThis.__gexCompositor.describe()');
      if (!d?.list.some((l) => / role=1 /.test(l) && l.endsWith(`"${title}"`))) return true;
      await new Promise((r) => setTimeout(r, 500));
    }
    return false;
  };
  await guestRun(`kill ${pids} 2>/dev/null; sleep 1`).catch(() => {});
  if (await gone(10)) return true;
  // foot blocks SIGTERM and takes it in epoll_pwait's mask, which the kernel
  // does not apply yet, so it never sees the signal. Said, then SIGKILL, so the
  // legs after this one get a desktop without a stray window over it.
  // A known failure until the kernel applies epoll_pwait's mask (routed to W);
  // DEMO_SIGTERM=hard makes it a hard check, which it becomes once that lands.
  const hard = process.env.DEMO_SIGTERM === 'hard';
  if (hard) failed = true;
  log(`${hard ? 'FAIL' : 'known failure:'} ${name} ignored SIGTERM (still running 5 s after it:`
    + ' the kernel does not apply epoll_pwait\'s signal mask); sending SIGKILL');
  await guestRun(`kill -9 ${pids} 2>/dev/null; sleep 1`).catch(() => {});
  if (await gone(20)) return true;
  failed = true;
  const ps = await guestRun('ps -eo pid,comm,args').catch(() => ({ lines: [] }));
  log(`FAIL ${name} exits: its window ${JSON.stringify(title)} is still there after SIGKILL; processes ${JSON.stringify(ps.lines?.map((l) => l.trim()).filter(Boolean))}`);
  return false;
}
/** A window's line in describe(), as numbers, with the page's scale. */
async function windowAt(title) {
  return ev(`(() => {
    const c = document.getElementById('out').getBoundingClientRect();
    const k = c.width / document.getElementById('out').width || 1;
    return globalThis.__gexCompositor.describe().then((d) => {
      const line = d.list.find((l) => / role=1 /.test(l) && l.endsWith(${JSON.stringify(`"${title}"`)}));
      if (!line) return null;
      const [w, h, x, y] = /(\\d+)x(\\d+)@(-?\\d+),(-?\\d+)/.exec(line).slice(1).map(Number);
      return { w, h, x, y, k, left: c.left, top: c.top, output: d.output };
    });
  })()`);
}
/**
 * The frame resizes a window: its bottom-right corner dragged by the pointer
 * (CDP), and the window's committed size -- what the client drew, not what
 * it was asked -- must grow, which is the client laying itself out again.
 * With "maximize": a double-click on the title bar fills the output, and a
 * second puts it back.
 */
async function resizeLeg(step) {
  const title = step.resize;
  const label = step.label ?? title;
  const a = await windowAt(title);
  if (!a) {
    failed = true;
    log(`FAIL ${label} resizes: no window titled ${JSON.stringify(title)}`);
    return;
  }
  const css = (dx, dy) => [a.left + dx * a.k, a.top + dy * a.k];
  // A corner just outside the frame, in the resize band, that is on the
  // page's canvas (the launcher is a page window and may cover one) and has
  // room to be dragged outwards: bottom-right first.
  const [vw, vh] = [await ev('innerWidth'), await ev('innerHeight')];
  const g = 3 / a.k;
  const corners = [
    { name: 'bottom-right', at: css(a.x + a.w + g, a.y + a.h + g), dir: [1, 1] },
    { name: 'bottom-left', at: css(a.x - g - 1, a.y + a.h + g), dir: [-1, 1] },
    { name: 'top-right', at: css(a.x + a.w + g, a.y - 26 / a.k - g), dir: [1, -1] },
    { name: 'top-left', at: css(a.x - g - 1, a.y - 26 / a.k - g), dir: [-1, -1] },
  ];
  let pick = null;
  for (const c of corners) {
    const [x, y] = c.at;
    const room = [c.dir[0] > 0 ? vw - x - 20 : x - 20, c.dir[1] > 0 ? vh - y - 20 : y - 20];
    const onCanvas = await ev(`document.elementFromPoint(${x}, ${y})?.id === 'out'`);
    if (onCanvas && room[0] >= 50 && room[1] >= 40) {
      pick = { ...c, room };
      break;
    }
  }
  if (!pick) {
    failed = true;
    log(`FAIL ${label} resizes: no corner of ${a.w}x${a.h}@${a.x},${a.y} is on the canvas with room to drag`);
    return;
  }
  const [sx, sy] = pick.at;
  const mouse = (type, x, y) => b.send('Input.dispatchMouseEvent',
    { type, x, y, button: 'left', buttons: type === 'mouseReleased' ? 0 : 1, clickCount: 1 }, s);
  await b.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x: sx, y: sy }, s);
  await new Promise((r) => setTimeout(r, 300));
  const cursor = (await ev("getComputedStyle(document.getElementById('out')).cursor")) || '';
  const under = (await ev('globalThis.__gexCompositor.describe()'))?.pointer ?? null;
  await mouse('mousePressed', sx, sy);
  const gx = pick.dir[0] * Math.min(160, pick.room[0]);
  const gy = pick.dir[1] * Math.min(100, pick.room[1]);
  for (let i = 1; i <= 8; i++) {
    await mouse('mouseMoved', sx + (gx * i) / 8, sy + (gy * i) / 8);
    await new Promise((r) => setTimeout(r, 120));
  }
  await mouse('mouseReleased', sx + gx, sy + gy);
  let z = null;
  for (let i = 0; i < 20; i++) {
    await new Promise((r) => setTimeout(r, 500));
    z = await windowAt(title);
    if (z && z.w > a.w + 20 && z.h > a.h + 20) break;
  }
  const grew = z && z.w > a.w + 20 && z.h > a.h + 20;
  if (!grew) failed = true;
  log(`${grew ? 'ok' : 'FAIL'} ${label} resizes: its ${pick.name} corner dragged ${Math.round(gx)},${Math.round(gy)} CSS pixels,`
    + ` ${a.w}x${a.h} became ${z ? `${z.w}x${z.h}` : 'nothing'} (cursor over the corner ${JSON.stringify(cursor)})`
    + `${grew ? '' : `; the compositor saw ${JSON.stringify(under)} under the pointer; windows ${JSON.stringify((await ev('globalThis.__gexCompositor.describe()'))?.list)}`}`);
  if (!step.maximize || !z) return;
  // The title bar, a double-click: the whole output, then back.
  const [tx, ty] = css(z.x + z.w / 3, z.y - 10);
  const dbl = async (x, y) => {
    await b.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x, y }, s);
    await new Promise((r) => setTimeout(r, 300));
    for (const c of [1, 2]) {
      await b.send('Input.dispatchMouseEvent', { type: 'mousePressed', x, y, button: 'left', buttons: 1, clickCount: c }, s);
      await b.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x, y, button: 'left', buttons: 0, clickCount: c }, s);
    }
  };
  await dbl(tx, ty);
  const [outW, outH] = z.output.split('x').map(Number);
  let m = null;
  for (let i = 0; i < 20; i++) {
    await new Promise((r) => setTimeout(r, 500));
    m = await windowAt(title);
    if (m && m.w >= outW * 0.9 && m.h >= outH * 0.8) break;
  }
  const maxed = m && m.w >= outW * 0.9 && m.h >= outH * 0.8 && m.w <= outW && m.h <= outH;
  await new Promise((r) => setTimeout(r, 500));
  if (m) await dbl(...css(m.x + m.w / 3, m.y - 10));
  let back = null;
  for (let i = 0; i < 20; i++) {
    await new Promise((r) => setTimeout(r, 500));
    back = await windowAt(title);
    if (back && Math.abs(back.w - z.w) < 24 && Math.abs(back.h - z.h) < 24) break;
  }
  const restored = back && Math.abs(back.w - z.w) < 24 && Math.abs(back.h - z.h) < 24;
  const ok = maxed && restored;
  if (!ok) failed = true;
  log(`${ok ? 'ok' : 'FAIL'} ${label} maximizes: a double-click on its title made it ${m ? `${m.w}x${m.h}` : 'nothing'}`
    + ` in a ${z.output} output, a second ${back ? `${back.w}x${back.h}` : 'nothing'} (was ${z.w}x${z.h})`);
}
/** The GPU's used memory in MiB (nvidia-smi), or null where there is none. */
let gpuSource = 'the card\'s total';
async function gpuMiB() {
  const { execFileSync } = await import('node:child_process');
  // This browser's own, when the driver says per process (nvidia-smi lists a
  // Vulkan/GL user as C+G): a correctness run shares the card with another
  // stream's browser, and the card's total is then theirs plus ours.
  try {
    const procs = execFileSync('nvidia-smi', ['--query-compute-apps=pid,used_memory', '--format=csv,noheader,nounits'], { encoding: 'utf8', timeout: 10000 })
      .trim().split('\n').filter(Boolean).map((l) => l.split(',').map((v) => Number(v.trim())));
    const ps = execFileSync('ps', ['-eo', 'pid=,ppid='], { encoding: 'utf8' }).trim().split('\n')
      .map((l) => l.trim().split(/\s+/).map(Number));
    const ours = new Set([chrome.pid]);
    for (let grew = true; grew;) {
      grew = false;
      for (const [pid, ppid] of ps) if (ours.has(ppid) && !ours.has(pid)) { ours.add(pid); grew = true; }
    }
    const mine = procs.filter(([pid]) => ours.has(pid));
    if (mine.length) gpuSource = 'this browser\'s processes';
    if (mine.length) return mine.reduce((t, [, mib]) => t + mib, 0);
  } catch { /* the card's total, below, if asked for */ }
  // The card's total counts every browser on it: only when this run has the
  // card alone (a timed bench run) and says so.
  if (process.env.DEMO_GPU_TOTAL !== '1') return null;
  try {
    const out = execFileSync('nvidia-smi', ['--query-gpu=memory.used', '--format=csv,noheader,nounits'], { encoding: 'utf8', timeout: 10000 });
    return Number(out.trim().split('\n')[0]);
  } catch {
    return null;
  }
}
/**
 * GPU memory must not grow per GL client: glxgears started from the launcher
 * and killed, N times, with the card's used memory read after each exit.
 * Fails if the last reading is more than `slack` MiB (default 64) above the
 * one after the first cycle, which is what a texture or context left behind
 * per client looks like on a 2 GB card that later loses its device.
 */
async function glCycles(step) {
  const n = step.glcycles;
  await ev(`globalThis.gexDesktop.install('gears')`);
  const seen = [await gpuMiB()];
  for (let i = 0; i < n; i++) {
    await ev(`globalThis.gexDesktop.launch('gears')`);
    await animates({ animates: 'glxgears', label: `glxgears cycle ${i + 1} animates`, wall: 60, samples: 3, min: 2 });
    await guestRun('kill $(pidof glxgears); sleep 2');
    await glForgotten(`glxgears cycle ${i + 1}`);
    seen.push(await gpuMiB());
    log(`  GPU memory after cycle ${i + 1}: ${seen.join(' -> ')} MiB`);
  }
  if (seen.some((v) => v === null)) {
    log(`ok GPU memory across ${n} GL clients: not measured here (nvidia-smi lists no process of this browser; DEMO_GPU_TOTAL=1 uses the card's total)`);
    return;
  }
  const slack = step.slack ?? 64;
  const grew = seen[n] - seen[1];
  const ok = grew <= slack;
  if (!ok) failed = true;
  log(`${ok ? 'ok' : 'FAIL'} GPU memory across ${n} GL clients: ${seen.join(' -> ')} MiB`
    + ` (before any, then after each exits; ${gpuSource}); ${grew} MiB since the first, ${slack} allowed`);
  // And one client animating: memory must not climb while it draws.
  const hold = step.hold ?? 30;
  if (!hold) return;
  await ev(`globalThis.gexDesktop.launch('gears')`);
  await animates({ animates: 'glxgears', label: 'glxgears (held) animates', wall: 60, samples: 2, min: 1 });
  const at = [await gpuMiB()];
  for (let t = 0; t < hold; t += 10) {
    await new Promise((r) => setTimeout(r, 10000));
    at.push(await gpuMiB());
  }
  await guestRun('kill $(pidof glxgears); sleep 2');
  if (at.some((v) => v === null)) {
    log(`ok GPU memory while one GL client animates: not measured here`);
    return;
  }
  const climbed = at[at.length - 1] - at[0];
  const held = climbed <= slack;
  if (!held) failed = true;
  log(`${held ? 'ok' : 'FAIL'} GPU memory while one GL client animates ${hold} s: ${at.join(' -> ')} MiB`
    + ` (every 10 s; ${gpuSource}); ${climbed} MiB, ${slack} allowed`);
}
/** Once a GL program has exited, the host's Mesa holds none of its windows.
 *  It used to keep every one: nothing told the compositor a process had gone,
 *  so a second glxgears, with the first one's X window id, matched both. */
async function glForgotten(who) {
  let held = null;
  for (let i = 0; i < 10; i++) {
    held = (await ev('globalThis.__gexCompositor.describe()'))?.glWindows ?? null;
    if (held && held.length === 0) break;
    await new Promise((r) => setTimeout(r, 500));
  }
  const ok = held !== null && held.length === 0;
  if (!ok) failed = true;
  if (held === null) await workerStacks();
  log(`${ok ? 'ok' : 'FAIL'} ${who} exited and the host's Mesa holds ${held === null ? 'an unknown number of' : held.length} GL windows${ok ? '' : `: ${held?.join(' ')}`}`);
}
/**
 * The X server frees what it no longer draws from: an xterm printing a line
 * every second or so gets a new pool per line (Xwayland drops a buffer unused
 * for a second), and before, each stayed -- about 0.9 MB a line.
 */
async function poolLeg(step) {
  const secs = step.seconds ?? 12;
  const p = await places();
  await click(...p.win);
  await new Promise((r) => setTimeout(r, 500));
  for (const ch of `\nrm -f /tmp/pool.done; for i in $(seq ${secs}); do echo pool$i; sleep 1; done; touch /tmp/pool.done\n`) await typeKey(ch);
  await new Promise((r) => setTimeout(r, 2500));
  const from = xPools;
  await new Promise((r) => setTimeout(r, secs * 1000));
  const to = xPools;
  const ok = from !== null && to !== null && to - from <= 2;
  if (!ok) failed = true;
  log(`${ok ? 'ok' : 'FAIL'} pool leg: the X server held ${from} pools, then ${to} after ${secs} s of the xterm printing a line a second`);
  // The loop's end, so whatever is typed next is a command of its own.
  await guestRun('for i in $(seq 60); do [ -e /tmp/pool.done ] && break; sleep 1; done');
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
  // no input reaches the page's canvas; what it draws as it is typed into is
  // what the compositor shows (the X server's copy against the output).
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
    } else if (step.eval) {
      log(`eval ${step.eval}: ${JSON.stringify(await ev(step.eval)).slice(0, 400)}`);
    } else if (step.gl) {
      await glLeg();
    } else if (step.fits) {
      await fitsLeg(step);
    } else if (step.resize) {
      await resizeLeg(step);
    } else if (step.glcycles) {
      await glCycles(step);
    } else if (step.animates) {
      await animates(step);
    } else if (step.truth) {
      await truth(step);
    } else if (step.pools) {
      await poolLeg(step);
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
    } else if (step.close) {
      if (await closeProgram(step.close, step.title ?? step.close)) log(`ok ${step.close} exits`);
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
// DEMO_STDERR=PATH: the browser's whole stderr, which has the Workers' console
// lines and the GPU process's own (a lost device says why only there).
if (process.env.DEMO_STDERR) (await import('node:fs')).writeFileSync(process.env.DEMO_STDERR, stderr);
const errs = stderr.split('\n').filter((l) => /Uncaught|RuntimeError|\[gex-console-error\]/.test(l));
for (const l of errs.slice(0, 10)) log(`stderr: ${l.slice(0, 300)}`);
b.close();
server.close();
cleanup();
process.exit(failed ? 1 : 0);
