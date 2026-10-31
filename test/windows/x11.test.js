// X11 with windows: the runtime's X server, started for the first X client,
// and that client's window handed to the page as any other.
import { System } from '../../web/runtime/src/index.js';
import { Filesystem } from '../../web/runtime/src/fs.js';
import { HostWindows } from '../../web/runtime/src/windows/host.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 120, html: `<style>html, body { margin: 0; } #desk { position: relative; width: 800px; height: 600px; }</style>
<div id="desk"></div>` };

export default async function (t) {
  const { check, eq, timeout, until, lines } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new Filesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  const hw = new HostWindows(sys, { width: 800, height: 600, scale: 1 });
  const desk = document.getElementById('desk');
  const wins = [];
  hw.addEventListener('window', (e) => {
    const w = e.window;
    wins.push(w);
    w.canvas.style.position = 'absolute';
    desk.append(w.canvas);
    w.place(0, 0);
  });
  await timeout(sys.boot(), 60000, 'boot');
  eq('env', hw.env.DISPLAY, ':0');
  const t0 = performance.now();
  const p = sys.spawn(['rt-x11', 'xhello'], { env: { PATH: '/bin', ...hw.env } });
  const out = lines(p);
  const err = lines(p, p.stderr);
  const w = await until(() => wins.find((x) => x.title === 'xhello'), 'an X window', 60000).catch((e) => {
    throw new Error(`${e.message}; stdout ${JSON.stringify(out)}, stderr ${JSON.stringify(err)}, windows ${JSON.stringify(wins.map((x) => [x.kind, x.title]))}, console ${JSON.stringify(sys.state.console.slice(-600))}`);
  });
  check(`the X server started for it (${(performance.now() - t0).toFixed(0)} ms to its window)`, true);
  eq('kind', w.kind, 'toplevel');
  eq('size', [w.width, w.height], [160, 80]);
  await until(() => {
    try { return w.canvas.getContext('2d').getImageData(5, 5, 1, 1).data[2] === 0xc0; } catch { return false; }
  }, 'its pixels');
  check('painted', true);
  w.focus();
  await new Promise((r) => setTimeout(r, 300));
  w.canvas.dispatchEvent(new KeyboardEvent('keydown', { code: 'KeyA', key: 'a', bubbles: true }));
  w.canvas.dispatchEvent(new KeyboardEvent('keyup', { code: 'KeyA', key: 'a', bubbles: true }));
  await until(() => out.includes('key 38'), 'a key').catch(() => {});
  check('a key reaches it', out.includes('key 38'), JSON.stringify(out));
  w.close();
  eq('close', await timeout(p.exited, 20000, 'close'), { code: 0, signal: null });

  // A second client: the same server.
  const q = sys.spawn(['rt-x11', 'again'], { env: { PATH: '/bin', ...hw.env } });
  const w2 = await until(() => wins.find((x) => x.title === 'again'), 'a second X window');
  w2.close();
  eq('a second client', await timeout(q.exited, 20000, 'second close'), { code: 0, signal: null });
  await sys.shutdown();
}
