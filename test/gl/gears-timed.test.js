// gles-gears' frame rate on the guest GL: 600 frames into a pbuffer through
// Mesa on WebGPU, after 30 to warm up, as `times` (ms for the 600, and frames
// per second). A timed test: bench.sh only (`--timed`), on the GPU.
import { System } from '../../web/runtime/src/index.js';
import { Filesystem } from '../../web/runtime/src/fs.js';
import { CompositorWindows } from '../../web/runtime/src/windows/compositor.js';
import { MesaGl } from '../../web/runtime/src/gl.js';

export const meta = { hosts: ['browser'], images: ['gl-probes'], timeout: 600, timed: true, html: '<canvas id="out" width="640" height="480"></canvas>' };

export default async function (t) {
  if (!(await navigator.gpu?.requestAdapter().catch(() => null))) return t.skip('no WebGPU adapter here');
  const tar = `gl-probes-${t.arch === 'x86' ? 'x86' : 'a64'}.tar`;
  if (!(await fetch(t.work(tar), { method: 'HEAD' })).ok) return t.skip(`no ${tar} (fixtures/images.mjs)`);
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  new Filesystem(sys).addTar(t.work(tar));
  new CompositorWindows(sys, { canvas: document.getElementById('out') });
  const gl = new MesaGl(sys);
  await t.timeout(sys.boot(), 60000, 'boot');
  await t.timeout(gl.start(), 120000, 'Mesa');
  const run = (n) => t.timeout(sys.run(['/probes/gles-gears', String(n)], { env: { HOME: '/' } }), 300000, `gles-gears ${n}`);
  // Each run pays the program's start and the context's: the difference
  // between a long and a short run is the frames alone.
  const timed = async (n) => {
    const t0 = performance.now();
    const r = await run(n);
    const ms = performance.now() - t0;
    t.eq(`${n} frames ran`, r.code, 0, r.stderr.slice(-200));
    t.record(`${n} frames ms`, Math.round(ms));
    return ms;
  };
  const ms = (await timed(630)) - (await timed(30));
  t.record('fps', Math.round((600 / ms) * 10000) / 10);
  t.say(`600 frames in ${Math.round(ms)} ms: ${Math.round((600 / ms) * 10000) / 10} frames per second`);
  await sys.shutdown();
}
