// A Windows program on 32-bit Wine in the page (web/nt.html, x86 shim, gl=1 on Mesa's
// gex_wgpu driver), the bench-box half of `tools/wine-wasm/run.sh --page`.
//
//   node tools/wine-wasm/wine32/page-run.mjs TREE SECS EXE [ARGS...]
//
// TREE is a page-ready tree (web/node_modules, web/public/gex's x86 shims and
// Mesa) whose web/public/gex/nt holds the bundles WINE32_PAGE_TARS names (comma
// separated page paths; run.sh's: the dist at /w3, then the prefix at /pfx,
// xkb data at /usr and the program's directory at /g). EXE is the program's
// file name in /g. Runs until it exits or for SECS seconds (0: until it
// exits), then prints the program's output tail, a "unitless: PATH BASE" line
// for each image that ran without a function-tier unit (run.sh --build-units
// builds them), and one result line:
//   "== w3 page: exited STATUS at T s (F frames)"
//   "== w3 page: running at SECS s (F frames)"   (a timeout is no failure)
//   "== w3 page: FAIL WHY"
// Env: WINE32_PAGE_SIZE (WxH, default 1280x720), WINE32_PAGE_ENV (more K=V,...),
// WINE32_PAGE_DEBUG (WINEDEBUG), WINE32_PAGE_Q (the page's query flags, default
// &cgen=1), WINE32_PAGE_SHOT (DIR: the compositor's last frame as DIR/end.ppm).
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const [tree, secsArg, exe, ...args] = process.argv.slice(2);
if (!tree || !exe) {
  console.log('usage: page-run.mjs TREE SECS EXE [ARGS...]');
  process.exit(2);
}
const secs = Number(secsArg) || 0;
const { openPage, sleep, stopAll } = await import(join(tree, 'web/test/browser-harness.mjs'));
const size = process.env.WINE32_PAGE_SIZE || '1280x720';
const [w, h] = size.split('x').map(Number);
const tars = process.env.WINE32_PAGE_TARS;
const argv = encodeURIComponent(JSON.stringify([exe, ...args]));
const env = encodeURIComponent(['WINEPREFIX=/pfx', `WINEDEBUG=${process.env.WINE32_PAGE_DEBUG || 'err+all,fixme-all'}`,
  'XDG_RUNTIME_DIR=/tmp', 'WAYLAND_DISPLAY=wayland-0', ...(process.env.WINE32_PAGE_ENV ? [process.env.WINE32_PAGE_ENV] : [])].join(','));
const { evaluate, state, consoleLines } = await openPage(
  `tar=${tars}&exe=/w3/lib/wine/i386-unix/wine&mtimes=1&cwd=/g&gl=1&glbackend=wgpu&size=${size}`
    + `&argv=${argv}&env=${env}${process.env.WINE32_PAGE_Q ?? '&cgen=1'}`,
  { width: w + 20, height: h + 100 },
);
const t0 = Date.now();
const at = () => ((Date.now() - t0) / 1000).toFixed(0);
const frames = async () => {
  const gl = (await evaluate('globalThis.__gexCompositor?.describe({ pixels: false })', true).catch(() => null))?.gl ?? '';
  return Number(/produced=(\d+)/.exec(gl)?.[1] ?? 0);
};
let s = null;
let last = 0;
for (;;) {
  s = await state().catch(() => null);
  if (s?.phase === 'done' || s?.phase === 'failed') break;
  if (consoleLines.includes('[harness] the page crashed')) { s = { ...s, phase: 'failed', error: 'the page crashed' }; break; }
  if (secs && Date.now() - t0 > secs * 1000) break;
  if (Date.now() - last > 30000) {
    last = Date.now();
    console.log(`${at()} s: ${s?.phase ?? 'loading'}, ${await frames()} frames`);
  }
  await sleep(1000);
}
const n = await frames();
if (process.env.WINE32_PAGE_SHOT) {
  try {
    const r = await evaluate('globalThis.__gexCompositor?.readoutBase64()', true);
    if (r) {
      const rgba = Buffer.from(r.b64, 'base64');
      const rgb = Buffer.alloc(r.w * r.h * 3);
      for (let i = 0, j = 0; i < rgba.length; i += 4, j += 3) { rgb[j] = rgba[i]; rgb[j + 1] = rgba[i + 1]; rgb[j + 2] = rgba[i + 2]; }
      mkdirSync(process.env.WINE32_PAGE_SHOT, { recursive: true });
      writeFileSync(join(process.env.WINE32_PAGE_SHOT, 'end.ppm'), Buffer.concat([Buffer.from(`P6\n${r.w} ${r.h}\n255\n`), rgb]));
    }
  } catch { /* no picture is no failure */ }
}
if (s?.out) console.log(s.out.slice(-3000));
const seen = new Set();
for (const l of consoleLines) {
  const m = /image (\S+) at base (0x[0-9a-f]+): no function-tier unit/.exec(l);
  if (m && !seen.has(m[1] + m[2])) { seen.add(m[1] + m[2]); console.log(`unitless: ${m[1]} ${m[2]}`); }
}
if (s?.phase === 'failed') console.log(`== w3 page: FAIL ${s.error}`);
else if (s?.phase === 'done') console.log(`== w3 page: exited ${s.status} at ${at()} s (${n} frames)`);
else console.log(`== w3 page: running at ${at()} s (${n} frames)`);
stopAll();
process.exit(s?.phase === 'failed' ? 1 : 0);
