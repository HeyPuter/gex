// An X client's EGL window on the page, for the window tests: the probe
// (/probes/NAME in the gl-probes image, x86) is spawned beside HostWindows,
// its window found by its title, its frames waited for and the page's pixel
// in the window checked to be the probe's magenta, its window 200x150 by then. `more(out)` may look at
// the probe's other lines once its frames are in.
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';
import { HostWindows } from '../../page/runtime/src/windows/host.js';
import { MesaGl } from '../../page/runtime/src/gl.js';

export async function glWindow(t, probe, more = () => {}) {
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
  const p = sys.spawn([`/probes/${probe}`, '30', '40'], { env: { HOME: '/', ...hw.env } });
  const out = lines(p);
  const err = lines(p, p.stderr);
  const said = () => `stdout ${JSON.stringify(out)}, stderr ${JSON.stringify(err.slice(-8))}, console ${JSON.stringify(sys.state.console.slice(-600))}`;
  const w = await until(() => wins.find((x) => x.title === probe), 'its X window', 60000)
    .catch((e) => { throw new Error(`${e.message}; ${said()}`); });
  await until(() => out.some((l) => l.startsWith('frames')), 'its frames', 120000)
    .catch((e) => { throw new Error(`${e.message}; ${said()}`); });
  eq('no GL error', out.find((l) => l.startsWith('frames')), 'frames 30 gl error 0');
  // (a probe may resize its window before its last frame)
  await until(() => w.width === 200 && w.height === 150, 'its window at 200x150', 10000).catch(() => {});
  eq('size', [w.width, w.height], [200, 150]);
  more(out);
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
