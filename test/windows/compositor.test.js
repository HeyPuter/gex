// CompositorWindows: the whole desktop in one canvas, drawn by the scene
// compositor on WebGPU. Says so and passes where the browser has no WebGPU.
import { System } from '../../web/runtime/src/index.js';
import { Filesystem } from '../../web/runtime/src/fs.js';
import { CompositorWindows } from '../../web/runtime/src/windows/compositor.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 120, html: `<style>html, body { margin: 0; }</style>
<canvas id="out" width="800" height="600"></canvas>` };

export default async function (t) {
  const { say, check, eq, timeout, until } = t;
  const adapter = await navigator.gpu?.requestAdapter().catch(() => null);
  if (!adapter) return t.skip('no WebGPU adapter here');
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new Filesystem(sys);
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
  p.kill();
  await p.exited;
  await sys.shutdown();
}
