// A GL window beside HostWindows: an X client's EGL window surface
// (/probes/gles-window, cleared to magenta), its frames presented into the
// window's own GL canvas over the page's window (dom-compositor.js
// `showGlCanvas`), and the page showing them, as the browser composited it.
// x86 only: gles-window is in that tree alone (it has the host's Xlib).
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';
import { HostWindows } from '../../page/runtime/src/windows/host.js';
import { MesaGl } from '../../page/runtime/src/gl.js';

export const meta = { hosts: ['browser'], arches: ['x86'], images: ['gl-probes'], timeout: 300, html: `<style>html, body { margin: 0; } #desk { position: relative; width: 800px; height: 600px; }</style>
<div id="desk"></div>` };

export default async function (t) {
  const { check, eq, until, lines } = t;
  const adapter = await navigator.gpu?.requestAdapter().catch(() => null);
  if (!adapter) return t.skip('no WebGPU adapter here');
  // A window's GL canvas is a WebGPU canvas in either build, and on software
  // WebGPU (headless runs) a WebGPU canvas's first texture loses every device
  // (compositor-worker.js `presentMode`).
  if (adapter.info?.isFallbackAdapter || adapter.info?.architecture === 'swiftshader') return t.skip('software WebGPU only: SwiftShader loses the device at the window\'s WebGPU canvas (it draws on a GPU)');
  if (!(await fetch(t.work('gl-probes-x86.tar'), { method: 'HEAD' })).ok) return t.skip('no gl-probes-x86.tar (fixtures/images.mjs)');
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  new MemoryFilesystem(sys).addTar(t.work('gl-probes-x86.tar'));
  const hw = new HostWindows(sys, { width: 800, height: 600, scale: 1 });
  new MesaGl(sys);
  const desk = document.getElementById('desk');
  const wins = [];
  hw.addEventListener('window', (e) => {
    const w = e.window;
    wins.push(w);
    w.canvas.style.position = 'absolute';
    desk.append(w.canvas);
    w.place(0, 0);
  });
  await t.timeout(sys.boot(), 60000, 'boot');
  const p = sys.spawn(['/probes/gles-window', '30', '40'], { env: { HOME: '/', ...hw.env } });
  const out = lines(p);
  const err = lines(p, p.stderr);
  const said = () => `stdout ${JSON.stringify(out)}, stderr ${JSON.stringify(err.slice(-8))}, console ${JSON.stringify(sys.state.console.slice(-600))}`;
  const w = await until(() => wins.find((x) => x.title === 'gles-window'), 'its X window', 60000)
    .catch((e) => { throw new Error(`${e.message}; ${said()}`); });
  eq('size', [w.width, w.height], [200, 150]);
  await until(() => out.some((l) => l.startsWith('frames')), 'its frames', 120000)
    .catch((e) => { throw new Error(`${e.message}; ${said()}`); });
  eq('no GL error', out.find((l) => l.startsWith('frames')), 'frames 30 gl error 0');
  const r = w.canvas.getBoundingClientRect();
  const at = [Math.round(r.left + 100), Math.round(r.top + 75)];
  let px = null;
  const shown = await until(async () => {
    px = await t.pixel(...at);
    return px[0] > 200 && px[1] < 60 && px[2] > 200;
  }, 'magenta on the page', 20000).then(() => true, () => false);
  check('the page shows its GL frame', shown, `pixel at ${at}: ${JSON.stringify(px)}; GL canvases ${document.querySelectorAll('canvas[id^="gex-gl-"]').length}; ${said()}`);
  p.kill?.();
  await sys.shutdown();
}
