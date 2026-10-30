// PointerLock: a click on a window whose program asks for a locked pointer
// takes the browser's lock, the mouse's movement reaches it as relative
// motion, and Escape gives it back. Clicks are real (run.mjs sends them).
// With data-windows="compositor" on the body (pointerlock-compositor.html),
// the same through the scene compositor.
import { System } from '../src/index.js';
import { Filesystem } from '../src/fs.js';
import { HostWindows } from '../src/windows/host.js';
import { CompositorWindows } from '../src/windows/compositor.js';
import { PointerLock } from '../src/input.js';
import { main, check, eq, timeout, until, lines, input, click, testDebug } from './lib.js';

const move = (x, y) => input({ method: 'Input.dispatchMouseEvent', type: 'mouseMoved', x, y, button: 'none', buttons: 0 });
const key = (type, k, code, vk) => ({ method: 'Input.dispatchKeyEvent', type, key: k, code, windowsVirtualKeyCode: vk });

main(async () => {
  const sys = new System({ baseUrl: '/', debug: testDebug() });
  const fs = new Filesystem(sys);
  fs.addTar('/__rt/bb.tar.zst');
  const scene = document.body.dataset.windows === 'compositor';
  if (scene && !(await navigator.gpu?.requestAdapter().catch(() => null))) {
    check('no WebGPU adapter here: not run', true);
    return;
  }
  const desk = document.getElementById('desk');
  const wins = [];
  let hw;
  // Where to click a window: its canvas's corner, or where the scene
  // compositor put it.
  let at;
  if (scene) {
    hw = new CompositorWindows(sys, { canvas: document.getElementById('out'), scale: 1 });
    at = async (title) => {
      const d = await hw.describe();
      const line = d?.list?.find((l) => l.endsWith(`"${title}"`));
      const m = line && /@(-?\d+),(-?\d+)/.exec(line);
      return m ? [Number(m[1]) + 20, Number(m[2]) + 20] : null;
    };
  } else {
    hw = new HostWindows(sys, { width: 800, height: 600, scale: 1 });
    hw.addEventListener('window', (e) => {
      const w = e.window;
      wins.push(w);
      Object.assign(w.canvas.style, { position: 'absolute', left: '0px', top: '0px' });
      desk.append(w.canvas);
      w.place(0, 0);
    });
    at = async (title) => (wins.find((w) => w.title === title) ? [50, 50] : null);
  }
  const lock = new PointerLock(sys);
  const said = [];
  lock.addEventListener('lock', () => said.push('lock'));
  lock.addEventListener('unlock', () => said.push('unlock'));
  await timeout(sys.boot(), 60000, 'boot');
  const env = { PATH: '/bin', ...hw.env };

  // A window that asks for no lock: a click is a click.
  const plain = sys.spawn(['rt-wl', 'plain'], { env });
  const plainOut = lines(plain);
  const [px, py] = await until(async () => plainOut.includes('up') && at('plain'), 'the plain window');
  await click(px, py);
  await until(() => plainOut.includes('button'), 'its click');
  check('a window that asks for none does not lock', !document.pointerLockElement && !lock.locked);
  plain.kill();
  await plain.exited;

  const p = sys.spawn(['rt-wl', 'game', 'lock'], { env });
  const out = lines(p);
  const err = lines(p, p.stderr);
  const [gx, gy] = await until(async () => out.includes('up') && at('game'), 'the game window')
    .catch((e) => { throw new Error(`${e.message}: ${JSON.stringify(out)} ${JSON.stringify(err)}`); });
  check('not locked before a click', !lock.locked && !out.includes('locked'));
  await click(gx, gy);
  await until(() => lock.locked, 'the lock', 5000);
  await until(() => out.includes('locked'), 'the program told', 5000);
  eq('the page locked the window\'s canvas', document.pointerLockElement === (scene ? document.getElementById('out') : wins.find((w) => w.title === 'game').canvas), true);
  await move(gx + 30, gy + 20);
  await move(gx + 70, gy + 10);
  // The movement so far, said after each event: both moves in, or the
  // last said at the time out.
  await until(() => out.includes('rel 70 10'), 'relative motion', 5000).catch(() => {});
  const last = out.filter((l) => /^rel /.test(l)).pop();
  eq('the movement, relative', last, 'rel 70 10');
  await input(key('rawKeyDown', 'Escape', 'Escape', 27), key('keyUp', 'Escape', 'Escape', 27));
  // Headless Chromium need not take Escape as the user's: release() is the
  // page's way out either way.
  if (lock.locked) lock.release();
  await until(() => !lock.locked && out.includes('unlocked'), 'the unlock', 5000);
  eq('events', said, ['lock', 'unlock']);
  // A persistent lock: the next click takes it again.
  await click(gx, gy);
  await until(() => lock.locked, 'locked again', 5000);
  lock.release();
  await until(() => !lock.locked, 'released', 5000);
  p.kill();
  await p.exited;
  await sys.shutdown();
});
