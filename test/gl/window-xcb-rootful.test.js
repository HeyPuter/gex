// A GL X client on a rootful X screen (CompositorWindows x11: 'rootful'),
// where no window of the scene is an X window: its frames must go into its
// window's own pixels on the screen (page/src/xgl.js, ports/xwayland/gexgl.c).
// /probes/gles-window-xcb-under clears its window magenta at (40, 30) with a
// green window over part of it: magenta in the window, green where the other
// covers it, neither outside. Then the same at 1100x617, for what the frames'
// copy back costs. On SwiftShader too: the frames are a texture of the
// compositor's, not a WebGPU canvas of the page's.
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';
import { CompositorWindows } from '../../page/runtime/src/windows/compositor.js';
import { MesaGl } from '../../page/runtime/src/gl.js';

export const meta = { hosts: ['browser'], arches: ['x86'], images: ['gl-probes'], timeout: 300, html: `<style>html, body { margin: 0; }</style>
<canvas id="out" width="1280" height="720" style="width: 1280px; height: 720px; display: block;"></canvas>` };

const magenta = (p) => p[0] > 200 && p[1] < 60 && p[2] > 200;
const green = (p) => p[0] < 60 && p[1] > 200 && p[2] < 60;

export default async function (t) {
  const { check, eq, until, lines, say } = t;
  const adapter = await navigator.gpu?.requestAdapter().catch(() => null);
  if (!adapter) return t.skip('no WebGPU adapter here');
  if (!(await fetch(t.work('gl-probes-x86.tar'), { method: 'HEAD' })).ok) return t.skip('no gl-probes-x86.tar (fixtures/images.mjs)');
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  new MemoryFilesystem(sys).addTar(t.work('gl-probes-x86.tar'));
  const canvas = document.getElementById('out');
  const out = new CompositorWindows(sys, { canvas, scale: 1, x11: 'rootful' });
  new MesaGl(sys);
  await t.timeout(sys.boot(), 60000, 'boot');
  const r = canvas.getBoundingClientRect();
  const at = (x, y) => [Math.round(r.left + x), Math.round(r.top + y)];

  const run = (args) => {
    const p = sys.spawn(['/probes/gles-window-xcb-under', ...args], { env: { HOME: '/', ...out.env } });
    const o = lines(p);
    const err = lines(p, p.stderr);
    const said = () => `stdout ${JSON.stringify(o.slice(-6))}, stderr ${JSON.stringify(err.slice(-8))}, console ${JSON.stringify(sys.state.console.slice(-600))}`;
    return { p, o, said };
  };

  // -- what shows where -----------------------------------------------------
  {
    const { p, o, said } = run(['30', '200', '150']);
    await until(() => o.some((l) => l.startsWith('swaps')), 'its frames', 120000)
      .catch((e) => { throw new Error(`${e.message}; ${said()}`); });
    eq('surface', o.find((l) => l.startsWith('surface')), 'surface 200x150');
    // The GL window is (40, 30) 200x150; the cover (140, 105) 100x80.
    const places = {
      'the GL window, uncovered': [at(80, 60), magenta],
      'its corner': [at(41, 31), magenta],
      'where the other window covers it': [at(190, 150), green],
      'outside both': [at(20, 20), (p) => !magenta(p) && !green(p)],
      'the cover past the GL window': [at(230, 175), green],
    };
    let seen = {};
    const ok = await until(async () => {
      seen = {};
      let all = true;
      for (const [name, [xy, want]] of Object.entries(places)) {
        const px = await t.pixel(...xy);
        seen[name] = px;
        if (!want(px)) all = false;
      }
      return all;
    }, 'the frame on the screen', 30000).then(() => true, () => false);
    for (const [name, [xy, want]] of Object.entries(places)) check(name, want(seen[name] ?? [0, 0, 0]), `pixel at ${xy}: ${JSON.stringify(seen[name])}; ${said()}`);
    if (!ok) await t.screenshot('rootful-gl');
    p.kill?.();
    await until(async () => !magenta(await t.pixel(...at(80, 60))), 'the window gone', 20000).catch(() => {});
  }

  // -- the copy's cost at 1100x617 -----------------------------------------
  {
    const none = { copies: 0, mapMs: 0, waitMs: 0, packMs: 0, puts: 0, serverMs: 0 };
    const before = (await out.describe())?.xgl ?? none;
    const { p, o, said } = run(['8', '1100', '617']);
    await until(() => o.includes('done'), 'the timed run', 120000)
      .catch((e) => { throw new Error(`${e.message}; ${said()}`); });
    const after = (await out.describe())?.xgl ?? none;
    const copies = after.copies - before.copies;
    const swaps = o.filter((l) => l.startsWith('swaps')).slice(1).map((l) => /swaps (\d+) in ([\d.]+)/.exec(l)).filter(Boolean)
      .map((m) => Number(m[1]) / Number(m[2]));
    const fps = swaps.length ? swaps.reduce((a, b) => a + b, 0) / swaps.length : 0;
    const each = (k) => (copies ? (after[k] - before[k]) / copies : 0).toFixed(2);
    say(`1100x617 over 8 s: ${copies} frames copied back (${(copies / 8).toFixed(1)}/s), each ${each('mapMs')} ms from submit to mapped, ${each('waitMs')} ms waiting for the server to take the one before, ${each('packMs')} ms written for it; ${after.puts - before.puts} put, ${((after.puts - before.puts) ? ((after.serverMs - before.serverMs) / (after.puts - before.puts)) : 0).toFixed(2)} ms each in the server (taken and put); format ${after.format}; the client swapped ${fps.toFixed(1)} frames/s`);
    check('frames copied back at 1100x617', copies > 0, said());
    p.kill?.();
  }
  await sys.shutdown();
}
