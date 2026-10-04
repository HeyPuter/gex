// A Windows program's window in a real browser (docs/nt-layer.md §10.2,
// milestone (b)): tests/nt/bin/<arch>/window.exe on gex's NT layer in the
// shared-memory browser shim, its window a toplevel in the page's compositor
// (web/nt.html), checked by pixels and by input.
//
//   node tests/nt/browser-gui.mjs [--arch x64|x86] [--gl] [--keep] [--shot DIR]
//
// --gl runs glwindow.exe instead: the same picture drawn with OpenGL in a
// captioned window's client area (the GL frame drawn by the compositor, or
// read back with GEX_NT_GL_READBACK=1 in GEX_NT_PAGE_ENV).
//
// Needs: the x86 shared shim, seed and kernel in web/public/gex
// (GEX_ARCH=x86 shims/browser/build.sh), web/node_modules (vite), a staged
// Wine prefix, and chromium; see browser-harness.mjs.
//
// The checks: the window shows its four quadrants and white centre; a click
// in the yellow quadrant reaches the program (it prints "click X Y") and its
// black mark appears there; Escape ends it with status 0.

import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { bundle, openPage, near, sleep, stopAll, log } from './browser-harness.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const argv = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = argv.indexOf(name);
  return i >= 0 ? argv[i + 1] : dflt;
};
const arch = opt('--arch', 'x64');
const keep = argv.includes('--keep');
const shotDir = opt('--shot', null);
const gl = argv.includes('--gl');
const name = gl ? 'glwindow' : 'window';
const title = gl ? 'gex gl window' : 'gex window';
let failed = 0;
const check = (name, ok, detail = '') => {
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${name}${detail ? ` (${detail})` : ''}`);
  if (!ok) failed++;
};

async function main() {
  const exe = join(HERE, 'bin', arch, `${name}.exe`);
  const tar = bundle(`${name}-${arch}`, arch, { [`t/${name}.exe`]: exe }, [exe]);
  const t0 = Date.now();
  const env = [...(gl ? ['gl=1&glbackend=wgpu'] : []), ...(process.env.GEX_NT_PAGE_ENV ? [`env=${process.env.GEX_NT_PAGE_ENV}`] : [])];
  const { browser, page, evaluate: eval_, until, shot } = await openPage(
    `tar=/gex/nt/${tar.split('/').pop()}&exe=/t/${name}.exe&args=120&size=800x600${env.map((e) => `&${e}`).join('')}${process.env.GEX_NT_PAGE_FLAGS ?? ''}`, { shotDir });
  await until((s) => /shown/.test(s.out), 300000, '"shown"');
  check('the program starts and paints', true, `${((Date.now() - t0) / 1000).toFixed(1)} s`);
  await sleep(1500);

  const desc = await eval_('globalThis.__gexCompositor?.describe()', true);
  const win = (desc?.list ?? []).map((l) => /role=1 (\d+)x(\d+)@(-?\d+),(-?\d+).*"(.*)"/.exec(l)).find((m) => m && m[5] === title);
  check('the compositor has the window', !!win, (desc?.list ?? []).join('; '));
  if (!win) {
    log(`compositor: ${desc ? JSON.stringify({ log: desc.log, gl: desc.gl, frames: desc.frames }) : 'no answer'}`);
    return;
  }
  let [ww, wh, wx, wy] = win.slice(1, 5).map(Number);
  // The client area: a captioned window's surface includes the frame the
  // program draws (borders all round, the caption on top).
  if (gl) {
    const side = (ww - 320) / 2;
    wx += side;
    wy += wh - 240 - side;
    ww = 320;
    wh = 240;
  }
  check('at its size', ww === 320 && wh === 240, `${ww}x${wh}`);
  if (gl) log(`compositor gl: ${desc?.gl}`);

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
  if (gl) {
    const f = /frames (\d+)/.exec(end.out);
    log(`${f ? f[1] : '?'} frames in ${((Date.now() - t0) / 1000).toFixed(1)} s`);
  }
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
