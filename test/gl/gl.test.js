// MesaGl: the guest's GL device, and Mesa on WebGPU started on demand.
// Says so and passes where the browser has no WebGPU, or the build has no
// Mesa (web/public/gex/gex-wgpu-replay.*).
import { System } from '../../web/runtime/src/index.js';
import { Filesystem } from '../../web/runtime/src/fs.js';
import { CompositorWindows } from '../../web/runtime/src/windows/compositor.js';
import { MesaGl } from '../../web/runtime/src/gl.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 120, html: `<canvas id="out" width="640" height="480"></canvas>` };

export default async function (t) {
  const { say, check, eq, timeout } = t;
  const adapter = await navigator.gpu?.requestAdapter().catch(() => null);
  if (!adapter) return t.skip('no WebGPU adapter here');
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new Filesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  const gl = new MesaGl(sys);
  new CompositorWindows(sys, { canvas: document.getElementById('out'), scale: 1 });
  check('a second throws', (() => { try { new MesaGl(sys); return false; } catch { return true; } })());
  await timeout(sys.boot(), 60000, 'boot');
  // A translated program's /dev/gex-gl (shims/browser): said, not checked.
  // The i386 test tree has none, under runProgram alone too.
  const dev = await sys.run(['ls', '/dev/gex-gl'], { env: { PATH: '/bin' } });
  say(`  ls /dev/gex-gl: ${dev.code ? dev.stderr.trim() : dev.stdout.trim()}`);
  check('Mesa is not fetched at boot', !performance.getEntriesByType('resource').some((e) => /gex-wgpu-replay/.test(e.name)));
  const have = await fetch('/gex/gex-wgpu-replay.js', { method: 'HEAD' }).then((r) => r.ok && !/html/.test(r.headers.get('content-type') ?? ''), () => false);
  if (!have) {
    await sys.shutdown();
    return t.skip('no Mesa in this build');
  }
  const t0 = performance.now();
  const d = await timeout(gl.start(), 120000, 'Mesa');
  check(`start(): ${d} (${Math.round(performance.now() - t0)} ms)`, /Mesa/.test(d), d);
  eq('one start', await gl.start(), d);
  await sys.shutdown();
}
