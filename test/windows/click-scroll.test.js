// A press in the top band of a page taller than its viewport: a 34 px bar
// above a 1280x720 output in a 1280x720 viewport, so the canvas runs past the
// bottom and focusing it could scroll the page. The press must reach the X
// client where the pointer was (the busybox image's rt-x11, "at": its window-
// local press and motion), and the page must not scroll (compositor.js onDown:
// focus() without preventScroll scrolled the page 34 px before the press was
// mapped, so the guest got it 34 px low). SwiftShader here; on a GPU through
// bench.sh.
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';
import { CompositorWindows } from '../../page/runtime/src/windows/compositor.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 180, html: `<style>html, body { margin: 0; } #bar { height: 34px; }</style>
<div id="bar"></div><canvas id="out" width="1280" height="720"></canvas>` };

export default async function (t) {
  const { check, eq, timeout, until, lines, cdp, input, say } = t;
  await cdp({ method: 'Emulation.setDeviceMetricsOverride', width: 1280, height: 720, deviceScaleFactor: 1, mobile: false });
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  new MemoryFilesystem(sys).addTar(t.work('bb.tar.zst'));
  const canvas = document.getElementById('out');
  const out = new CompositorWindows(sys, { canvas });
  await timeout(sys.boot(), 60000, 'boot');
  const p = sys.spawn(['rt-x11', 'clicked', 'at'], { env: { PATH: '/bin', HOME: '/root', ...out.env } });
  const said = lines(p);
  let win = null;
  await until(async () => {
    if (!said.includes('mapped')) return false;
    const d = await out.describe();
    const m = (d?.list ?? []).map((l) => / 160x80@(-?\d+),(-?\d+)/.exec(l)).find(Boolean);
    if (m) win = { x: Number(m[1]), y: Number(m[2]) };
    return !!win;
  }, 'the X window', 60000).catch((e) => { throw new Error(`${e.message}: ${JSON.stringify(said)} ${JSON.stringify(sys.state.console.slice(-400))}`); });
  await new Promise((r) => setTimeout(r, 1000));
  const r = canvas.getBoundingClientRect();
  check(`the page is taller than the viewport and not scrolled (${document.scrollingElement.scrollHeight}, scrollY ${scrollY})`, document.scrollingElement.scrollHeight > 720 && scrollY === 0);
  // 12 px into the window's top band.
  const want = { x: 40, y: 12 };
  const x = r.left + (win.x + want.x) * (r.width / canvas.width);
  const y = r.top + (win.y + want.y) * (r.height / canvas.height);
  say(`  window at ${win.x},${win.y}; pointer to ${x},${y}`);
  const before = said.length;
  await input({ method: 'Input.dispatchMouseEvent', type: 'mouseMoved', x, y, button: 'none', buttons: 0 });
  const moved = await until(() => said.slice(before).map((l) => /^motion (-?\d+) (-?\d+)$/.exec(l)).filter(Boolean).pop(), 'a motion line', 3000).catch(() => null);
  await input(
    { method: 'Input.dispatchMouseEvent', type: 'mousePressed', x, y, button: 'left', buttons: 1, clickCount: 1 },
    { method: 'Input.dispatchMouseEvent', type: 'mouseReleased', x, y, button: 'left', buttons: 0, clickCount: 1 },
  );
  const m = await until(() => said.slice(before).map((l) => /^press 1 at (-?\d+) (-?\d+)$/.exec(l)).find(Boolean), 'the press', 10000).catch(() => null);
  const press = m && { x: Number(m[1]), y: Number(m[2]) };
  // Within 1 px of the spot aimed at (the window's 1 px border), and where the
  // guest last saw the pointer.
  check(`the press reached the guest at the spot pressed (${press ? `${press.x},${press.y}` : 'no press'}, aimed at ${want.x},${want.y})`,
    !!press && Math.abs(press.x - want.x) <= 1 && Math.abs(press.y - want.y) <= 1, JSON.stringify(said.slice(before)));
  if (moved) eq('the press is where the guest saw the pointer', press && [press.x, press.y], [Number(moved[1]), Number(moved[2])]);
  eq('window.scrollY stays 0', scrollY, 0);
  p.kill('SIGKILL');
  await sys.shutdown();
}
