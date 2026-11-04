// CompositorWindows: the whole desktop in one canvas, drawn by the scene
// compositor on WebGPU. Says so and passes where the browser has no WebGPU.
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';
import { CompositorWindows } from '../../page/runtime/src/windows/compositor.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 120, html: `<style>html, body { margin: 0; }</style>
<canvas id="out" width="800" height="600"></canvas>` };

export default async function (t) {
  const { say, check, eq, timeout, until } = t;
  const adapter = await navigator.gpu?.requestAdapter().catch(() => null);
  if (!adapter) return t.skip('no WebGPU adapter here');
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  const out = new CompositorWindows(sys, { canvas: document.getElementById('out'), scale: 1 });
  await timeout(sys.boot(), 60000, 'boot');
  const p = sys.spawn(['rt-wl', 'scene'], { env: { PATH: '/bin', ...out.env } });
  const d = await until(async () => {
    const v = await out.describe();
    return v?.titles?.includes('scene') ? v : null;
  }, 'its window in the scene', 60000).catch((e) => { throw new Error(`${e.message}: ${JSON.stringify(sys.state.console.slice(-400))}`); });
  eq('a window', d.windows >= 1, true);
  check('drawn', (d.frames ?? 0) > 0, JSON.stringify(d).slice(0, 300));
  // The window's colour on the screen, as the browser composited it: rt-wl
  // fills its 200x100 buffer with 0xff3060c0. Present is a frame or two
  // behind the scene, so asked until it shows.
  const want = [0x30, 0x60, 0xc0];
  const near = (px, i) => want.every((w, k) => Math.abs(px[i + k] - w) <= 8);
  let seen = 0;
  await until(async () => {
    const img = await t.pixels({ x: 0, y: 0, width: 800, height: 600 });
    seen = 0;
    for (let i = 0; i < img.data.length; i += 4) if (near(img.data, i)) seen++;
    return seen >= 200 * 100 * 0.9;
  }, 'its colour on the screen', 20000).catch(() => {});
  check(`its pixels on the screen (${seen})`, seen >= 200 * 100 * 0.9, `${seen} of 20000 pixels near ${want}`);
  if (seen < 200 * 100 * 0.9) await t.screenshot('compositor');
  p.kill();
  await p.exited;
  await sys.shutdown();
}
