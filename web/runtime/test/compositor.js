// CompositorWindows: the whole desktop in one canvas, drawn by the scene
// compositor on WebGPU. Says so and passes where the browser has no WebGPU.
import { System } from '../src/index.js';
import { Filesystem } from '../src/fs.js';
import { CompositorWindows } from '../src/windows/compositor.js';
import { main, say, check, eq, timeout, until, testDebug } from './lib.js';

main(async () => {
  const adapter = await navigator.gpu?.requestAdapter().catch(() => null);
  if (!adapter) {
    say('no WebGPU adapter here: not run');
    return;
  }
  const sys = new System({ baseUrl: '/', debug: testDebug() });
  const fs = new Filesystem(sys);
  fs.addTar('/__rt/bb.tar.zst');
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
});
