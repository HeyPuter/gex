// HostWindows: each guest window handed to the page, framed and placed by it.
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';
import { HostWindows } from '../../page/runtime/src/windows/host.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 120, html: `<style>html, body { margin: 0; } #desk { position: relative; width: 800px; height: 600px; }</style>
<div id="desk"></div>` };

export default async function (t) {
  const { say, check, eq, timeout, until, lines } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  const hw = new HostWindows(sys, { width: 800, height: 600, scale: 1 });
  check('a second windows part throws', (() => { try { new HostWindows(sys); return false; } catch { return true; } })());
  const desk = document.getElementById('desk');
  const wins = [];
  hw.addEventListener('window', (e) => {
    const w = e.window;
    wins.push(w);
    w.canvas.style.position = 'absolute';
    w.canvas.style.left = '10px';
    w.canvas.style.top = '10px';
    desk.append(w.canvas);
    w.place(10, 10);
  });
  await timeout(new Promise((r) => requestAnimationFrame(r)), 5000, 'an animation frame');
  await timeout(sys.boot(), 60000, 'boot');
  const env = { PATH: '/bin', ...hw.env };

  const p = sys.spawn(['rt-wl', 'hello', 'move'], { env });
  const out = lines(p);
  const err = lines(p, p.stderr);
  const w = await until(() => wins.find((x) => x.title === 'hello'), 'a window').catch((e) => {
    throw new Error(`${e.message}; its stdout ${JSON.stringify(out)}, stderr ${JSON.stringify(err)}, windows ${wins.length}, desktop ${!!globalThis.__gexDesktop}, records ${JSON.stringify(globalThis.__gexDesktop?.records?.().map((r) => [r.role, r.title, r.width, r.height, r.addr]))}, console ${JSON.stringify(sys.state.console.slice(-600))}`);
  });
  eq('kind', w.kind, 'toplevel');
  eq('decorations', w.decorations, 'host');
  eq('pid', w.pid, await p.started);
  eq('process windows', p.windows().length, 1);
  eq('size', [w.width, w.height], [200, 100]);
  await until(() => {
    try { return w.canvas.getContext('2d').getImageData(5, 5, 1, 1).data[2] === 0xc0; } catch { return false; }
  }, 'its pixels');
  check('painted', true);

  w.focus();
  await until(() => out.includes('focus'), 'keyboard focus');
  // Real input, through the browser to the focused window.
  await t.key('KeyA', 'a');
  await until(() => out.includes('key 30'), 'a key');
  check('a key reaches it', true);
  // The browser repeats a held key, and the compositor passes each repeat on
  // as a press: the client is told not to repeat itself (rate 0), so a
  // release the page is late handing on (its main thread busy) repeats
  // nothing. KEY_B is 48.
  eq('the client is told the browser repeats keys (rate 0)', out.find((l) => l.startsWith('repeat ')), 'repeat 0 600');
  const key = (type, autoRepeat = false) => t.input({ method: 'Input.dispatchKeyEvent', type, code: 'KeyB', key: 'b', windowsVirtualKeyCode: 66, autoRepeat });
  const bs = () => out.filter((l) => l === 'key 48').length;
  await key('keyDown');
  const end = performance.now() + 900;
  while (performance.now() < end);
  await key('keyUp');
  await until(() => bs() === 1, 'B');
  await new Promise((r) => setTimeout(r, 500));
  eq('a key held through a 900 ms stall is one press', bs(), 1);
  await key('keyDown');
  for (let i = 0; i < 5; i++) {
    await new Promise((r) => setTimeout(r, 50));
    await key('keyDown', true);
  }
  await key('keyUp');
  await until(() => bs() === 7, 'the browser\'s repeats of B');
  eq('a held key: its press and the browser\'s 5 repeats, 6 presses', bs() - 1, 6);

  const resized = new Promise((r) => w.addEventListener('resized', r, { once: true }));
  w.resize(300, 150);
  await timeout(resized, 20000, 'resized');
  eq('resize', [w.width, w.height, w.canvas.style.width], [300, 150, '300px']);

  w.setStates({ maximized: true }, 780, 580);
  await until(() => out.some((l) => /^configure 780 580 m/.test(l)), 'maximized configure');
  check('setStates', true);

  // A press on it: the client asks to be moved (its own title bar would).
  const moved = new Promise((r) => w.addEventListener('request', (e) => r(e.kind), { once: true }));
  const r = w.canvas.getBoundingClientRect();
  await t.move(r.left + 20, r.top + 20);
  await t.click(r.left + 20, r.top + 20);
  eq('move request', await timeout(moved, 20000, 'move request').catch((e) => `${e.message}: ${JSON.stringify(out.slice(-6))}`), 'move');

  const closed = new Promise((r) => w.addEventListener('closed', r, { once: true }));
  w.close();
  eq('close', await timeout(p.exited, 20000, 'close'), { code: 0, signal: null });
  await timeout(closed, 20000, 'closed');
  check('closed', !w.canvas.isConnected && hw.windows().length === 0);

  // A client that asks to be maximized.
  const q = sys.spawn(['rt-wl', 'max', 'maximize'], { env });
  const m = await until(() => wins.find((x) => x.title === 'max'), 'the second window');
  const reqd = new Promise((res) => m.addEventListener('request', (e) => res(e.kind), { once: true }));
  const r2 = m.canvas.getBoundingClientRect();
  await t.move(r2.left + 20, r2.top + 20);
  await t.click(r2.left + 20, r2.top + 20);
  const req = await timeout(reqd, 20000, 'maximize request');
  eq('maximize request', req, 'maximize');
  q.kill();
  await q.exited;

  say(JSON.stringify(sys.describe()));
  await sys.shutdown();
}
