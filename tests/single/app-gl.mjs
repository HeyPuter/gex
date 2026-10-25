// The single-thread build's GL through app.js (the library eject pages and
// nt.html start programs with): gles-gears in nt.html with single=1, drawn by
// the single-thread Mesa (web/public/gex/st/). The demo page's gears
// (web/test/site.mjs) starts its program without app.js, so it cannot see
// what app.js hands the system Worker. Prints `ok   sp app gl` or FAIL lines.
//
//   node tests/single/app-gl.mjs     (web/public/gex/st/ built: web/build-all.sh --sp or --both;
//                                     a GPU, or GEX_WEBGPU_SOFTWARE=1, the default, SwiftShader)
//
// The guest files are the gears demo's, from SP_GL_FS (default
// web/public/fs/x86, web/build-assets.sh's). GEX_NT_VITE_PORT /
// GEX_NT_CHROME_PORT pick the ports (default 5295, 9435).
import { existsSync, mkdirSync, rmSync, copyFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
process.env.GEX_NT_VITE_PORT ??= '5295';
process.env.GEX_NT_CHROME_PORT ??= '9435';
const { openPage, sleep, log, WEB, STATE, stopAll } = await import('../nt/browser-harness.mjs');

const fail = (why) => { console.log(`FAIL sp app gl: ${why}`); stopAll(); process.exit(1); };
const FS = process.env.SP_GL_FS || join(ROOT, 'web/public/fs/x86');
const need = ['gles-gears', 'ld-musl-x86_64.so.1', 'libEGL.so.1', 'libGLESv2.so.2'];
for (const f of need) if (!existsSync(join(FS, f))) fail(`no ${join(FS, f)} (web/build-assets.sh)`);
if (!existsSync(join(WEB, 'public/gex/st/gex-wgpu-replay.wasm'))) fail('no web/public/gex/st Mesa (web/build-all.sh --sp)');

// The demo's layout: the loader in /lib, the GL libraries under both names.
const stage = join(STATE, 'stage-sp-app-gl');
rmSync(stage, { recursive: true, force: true });
for (const d of ['game', 'lib', 'usr/lib', 'tmp']) mkdirSync(join(stage, d), { recursive: true });
copyFileSync(join(FS, 'gles-gears'), join(stage, 'game/gles-gears'));
copyFileSync(join(FS, 'ld-musl-x86_64.so.1'), join(stage, 'lib/ld-musl-x86_64.so.1'));
for (const [f, link] of [['libEGL.so.1', 'libEGL.so'], ['libGLESv2.so.2', 'libGLESv2.so']]) {
  copyFileSync(join(FS, f), join(stage, 'usr/lib', f));
  copyFileSync(join(FS, f), join(stage, 'usr/lib', link));
}
const tar = join(WEB, 'public/gex/nt/sp-app-gl.tar');
mkdirSync(dirname(tar), { recursive: true });
execFileSync('tar', ['-cf', tar, '-C', stage, 'game', 'lib', 'usr', 'tmp']);

const query = 'tar=/gex/nt/sp-app-gl.tar&exe=/game/gles-gears&args=200&cwd=/game&gl=1&glbackend=wgpu&size=256x192&single=1';
let opened;
try {
  opened = await openPage(query, { width: 400, height: 300 });
} catch (e) { fail(String(e?.message ?? e)); }
const { state, consoleLines } = opened;
const end = Date.now() + 5 * 60000;
for (;;) {
  const s = await state();
  if (s?.phase === 'done' || s?.phase === 'failed') {
    const out = s.out ?? '';
    const covered = /covered ([1-9]\d*)%/.exec(out);
    if (s.phase === 'done' && s.status === 0 && covered && /single-thread build/.test(out)) {
      log(`covered ${covered[1]}%`);
      console.log('ok   sp app gl');
      stopAll();
      process.exit(0);
    }
    log(`output: ${out.slice(-2000)}`);
    fail(`the program ended (${s.phase}, status ${s.status}) ${s.error ?? ''}`);
  }
  const died = consoleLines.find((l) => /\[gex\] pid \d+:\d+ threw:|\[harness\] the page crashed/.test(l));
  if (died) fail(died.slice(0, 600));
  if (Date.now() > end) fail(`no exit in time; output: ${(s?.out ?? '').slice(-2000)}`);
  await sleep(500);
}
