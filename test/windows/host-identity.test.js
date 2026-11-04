// HostWindows as a web desktop's taskbar and title bars read them: a window's
// whole title, its application (`appId`, `appid`), its program (`pid`, an X
// window's from _NET_WM_PID) and a client's own minimize (`request`).
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';
import { HostWindows } from '../../page/runtime/src/windows/host.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 180, html: `<style>html, body { margin: 0; } #desk { position: relative; width: 800px; height: 600px; }</style>
<div id="desk"></div>` };

// Past the 32 bytes a record once carried, with a character of two bytes and
// one of three.
const LONG = 'Document 1 — a title much longer than thirty-two bytes, café';
const XLONG = 'xterm-like title, longer than thirty-two bytes';

export default async function (t) {
  const { eq, check, timeout, until, lines } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  const hw = new HostWindows(sys, { width: 800, height: 600, scale: 1 });
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
  await timeout(sys.boot(), 60000, 'boot');
  const env = { PATH: '/bin', ...hw.env };

  // A Wayland window: its title whole, its app id, its own minimize button.
  const p = sys.spawn(['rt-wl', LONG, 'minimize', 'app', 'org.example.Probe'], { env });
  const out = lines(p);
  const w = await until(() => wins.find((x) => x.kind === 'toplevel' && x.title.startsWith('Document 1')), 'a Wayland window').catch((e) => {
    throw new Error(`${e.message}; stdout ${JSON.stringify(out)}, windows ${JSON.stringify(wins.map((x) => x.title))}`);
  });
  eq('the whole title', w.title, LONG);
  await until(() => w.appId === 'org.example.Probe', 'its app id').catch(() => {});
  eq('app id', w.appId, 'org.example.Probe');
  eq('pid', w.pid, await p.started);
  const asked = new Promise((r) => w.addEventListener('request', (e) => r(e.kind), { once: true }));
  const r = w.canvas.getBoundingClientRect();
  await t.click(r.left + 20, r.top + 20);
  eq('minimize request', await timeout(asked, 20000, 'a request').catch((e) => `${e.message}: ${JSON.stringify(out.slice(-6))}`), 'minimize');
  w.close();
  await timeout(p.exited, 20000, 'close');

  // A window that says nothing of its application.
  const q = sys.spawn(['rt-wl', 'plain'], { env });
  const plain = await until(() => wins.find((x) => x.title === 'plain'), 'a plain window');
  eq('no app id', plain.appId, null);
  plain.close();
  await timeout(q.exited, 20000, 'close');

  // An X window: its program by _NET_WM_PID (its connection is the X
  // server's), its application by WM_CLASS.
  const x = sys.spawn(['rt-x11', XLONG], { env });
  const xout = lines(x);
  const xw = await until(() => wins.find((v) => v.title === XLONG), 'an X window', 60000).catch((e) => {
    throw new Error(`${e.message}; stdout ${JSON.stringify(xout)}, windows ${JSON.stringify(wins.map((v) => v.title))}, console ${JSON.stringify(sys.state.console.slice(-600))}`);
  });
  check('an X window\'s whole title', true);
  const xpid = await x.started;
  await until(() => xw.pid === xpid && xw.appId === 'RtX11', 'its pid and class').catch(() => {});
  eq('X window pid', xw.pid, xpid);
  eq('X window app id', xw.appId, 'RtX11');
  eq('its process lists it', x.windows().map((v) => v.title), [XLONG]);
  xw.close();
  eq('X close', await timeout(x.exited, 20000, 'X close'), { code: 0, signal: null });
  await sys.shutdown();
}
