// X programs through Xwayland: the clipboard both ways (the X window
// manager's selection bridge) and a game's pointer grab (hidden cursor and
// warps), which Xwayland turns into a locked pointer and relative motion.
import { System } from '../src/index.js';
import { Filesystem } from '../src/fs.js';
import { HostWindows } from '../src/windows/host.js';
import { Clipboard, PointerLock } from '../src/input.js';
import { main, check, eq, timeout, until, lines, input, click, testDebug } from './lib.js';

const move = (x, y) => input({ method: 'Input.dispatchMouseEvent', type: 'mouseMoved', x, y, button: 'none', buttons: 0 });

main(async () => {
  const sys = new System({ baseUrl: '/', debug: testDebug() });
  const fs = new Filesystem(sys);
  fs.addTar('/__rt/bb.tar.zst');
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
  check('it owns the clipboard', writer.out.includes('copied'), JSON.stringify(writer.out));
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
  for (const x of [reader, writer, game]) x.p.kill();
  await Promise.all([reader, writer, game].map((x) => x.p.exited));
  await sys.shutdown();
});
