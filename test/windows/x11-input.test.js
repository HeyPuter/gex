// X programs through Xwayland: the clipboard both ways (the X window
// manager's selection bridge), a game's pointer grab (hidden cursor and
// warps), which Xwayland turns into a locked pointer and relative motion,
// and the keyboard's repeat, which is the browser's: a key held while the
// page's main thread is busy is one key (no repeat timer in Xwayland runs
// on past a release the page has not handed on yet), and the browser's own
// repeats while a key is held are presses, the release after them.
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';
import { HostWindows } from '../../page/runtime/src/windows/host.js';
import { Clipboard, PointerLock } from '../../page/runtime/src/input.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 120, html: `<style>html, body { margin: 0; } #desk { position: relative; width: 800px; height: 600px; }</style>
<div id="desk"></div>` };


export default async function (t) {
  const { check, eq, timeout, until, lines, input, click } = t;
  const move = (x, y) => input({ method: 'Input.dispatchMouseEvent', type: 'mouseMoved', x, y, button: 'none', buttons: 0 });
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  const hw = new HostWindows(sys, { width: 800, height: 600, scale: 1 });
  const desk = document.getElementById('desk');
  const wins = [];
  let left = 0;
  // Where each window's canvas went.
  const at = new Map();
  hw.addEventListener('window', (e) => {
    const w = e.window;
    wins.push(w);
    at.set(w, left);
    Object.assign(w.canvas.style, { position: 'absolute', left: `${left}px`, top: '0px' });
    desk.append(w.canvas);
    w.place(left, 0);
    left += 200;
  });
  const clip = new Clipboard(sys, { sync: false });
  const copies = [];
  clip.addEventListener('copy', (e) => copies.push(e.text));
  const lock = new PointerLock(sys);
  await timeout(sys.boot(), 60000, 'boot');
  const env = { PATH: '/bin', ...hw.env };
  const start = async (title, ...args) => {
    const p = sys.spawn(['rt-x11', title, ...args], { env });
    const out = lines(p);
    const err = lines(p, p.stderr);
    const w = await until(() => out.includes('mapped') && wins.find((x) => x.title === title), `the X window ${title}`, 60000)
      .catch((e) => { throw new Error(`${e.message}: ${JSON.stringify(out)} ${JSON.stringify(err)} ${JSON.stringify(sys.state.console.slice(-400))}`); });
    return { p, out, w };
  };

  // The page's text, pasted by an X program.
  const reader = await start('xreader', 'paste');
  clip.setText('to X ✓');
  await new Promise((r) => setTimeout(r, 300));
  await click(at.get(reader.w) + 20, 20);
  await until(() => reader.out.some((l) => l.startsWith('paste')), 'the X paste', 10000)
    .catch((e) => { throw new Error(`${e.message}: ${JSON.stringify(reader.out)}`); });
  eq('an X program pastes the page\'s text', reader.out.find((l) => l.startsWith('paste')), 'paste to X ✓');

  // An X program's copy, out to the page.
  const writer = await start('xwriter', 'copy', 'from X');
  check('it owns the clipboard', await until(() => writer.out.includes('copied'), 'copied', 10000).then(() => true, () => false), JSON.stringify(writer.out));
  await until(() => copies.length, 'the X copy', 10000)
    .catch((e) => { throw new Error(`${e.message}: ${JSON.stringify(writer.out)}`); });
  eq('an X program\'s copy comes out', copies, ['from X']);

  // Mouse look: grab, hide, warp.
  const game = await start('xgame', 'look');
  await click(at.get(game.w) + 20, 20);
  await until(() => game.out.some((l) => l.startsWith('grabbed')), 'the grab', 10000);
  eq('the grab', game.out.find((l) => l.startsWith('grabbed')), 'grabbed 0');
  await until(() => lock.locked, 'Xwayland locking the pointer', 10000)
    .catch((e) => { throw new Error(`${e.message}: ${JSON.stringify(game.out)}`); });
  check('Xwayland locked the pointer', true);
  await move(at.get(game.w) + 50, 30);
  await until(() => game.out.some((l) => l.startsWith('look')), 'mouse look', 10000)
    .catch((e) => { throw new Error(`${e.message}: ${JSON.stringify(game.out)}`); });
  check(`relative motion reaches the X program (${game.out.filter((l) => l.startsWith('look')).join(', ')})`, true);
  lock.release();

  // The keyboard's repeat. KEY_A is X keycode 38, KEY_B 56.
  const keys = await start('xkeys');
  await click(at.get(keys.w) + 20, 20);
  const key = (type, code, autoRepeat = false) => input({ method: 'Input.dispatchKeyEvent', type, code, key: code.slice(3).toLowerCase(),
    windowsVirtualKeyCode: code.charCodeAt(3), autoRepeat });
  const of = (k) => keys.out.filter((l) => l === `key ${k}` || l === `release ${k}`);
  await key('keyDown', 'KeyA');
  // the page's main thread busy for 900 ms before the release is handed on
  const end = performance.now() + 900;
  while (performance.now() < end);
  await key('keyUp', 'KeyA');
  await until(() => of(38).includes('release 38'), 'the release of A', 10000)
    .catch((e) => { throw new Error(`${e.message}: ${JSON.stringify(keys.out)}`); });
  await new Promise((r) => setTimeout(r, 1000));
  eq('a key held through a 900 ms stall is one key', of(38), ['key 38', 'release 38']);
  await key('keyDown', 'KeyB');
  for (let i = 0; i < 5; i++) {
    await new Promise((r) => setTimeout(r, 50));
    await key('keyDown', 'KeyB', true);
  }
  await key('keyUp', 'KeyB');
  await until(() => of(56).at(-1) === 'release 56', 'the release of B', 10000)
    .catch((e) => { throw new Error(`${e.message}: ${JSON.stringify(keys.out)}`); });
  await new Promise((r) => setTimeout(r, 500));
  const b = of(56);
  eq('a held key repeats as the browser repeats it: 6 presses', b.filter((l) => l === 'key 56').length, 6);
  eq('and its release comes after them', b.at(-1), 'release 56');

  for (const x of [reader, writer, game, keys]) x.p.kill();
  await Promise.all([reader, writer, game, keys].map((x) => x.p.exited));
  await sys.shutdown();
}
