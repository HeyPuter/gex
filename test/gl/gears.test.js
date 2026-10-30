// gles-gears on the guest GL: an unmodified GLES2 program drawing 30 frames
// into a pbuffer through Mesa on WebGPU, then reading the last one back. It
// must draw as much of the frame as it does on the host's own Mesa
// (fixtures/images.mjs `gl-probes`), with no GL error.
import { System } from '../../web/runtime/src/index.js';
import { Filesystem } from '../../web/runtime/src/fs.js';
import { CompositorWindows } from '../../web/runtime/src/windows/compositor.js';
import { MesaGl } from '../../web/runtime/src/gl.js';

export const meta = { hosts: ['browser'], images: ['gl-probes'], timeout: 300, html: '<canvas id="out" width="640" height="480"></canvas>' };

export default async function (t) {
  if (!(await navigator.gpu?.requestAdapter().catch(() => null))) return t.skip('no WebGPU adapter here');
  const tar = `gl-probes-${t.arch === 'x86' ? 'x86' : 'a64'}.tar`;
  if (!(await fetch(t.work(tar), { method: 'HEAD' })).ok) return t.skip(`no ${tar} (fixtures/images.mjs)`);
  const want = (await (await fetch(t.work('gl-probes.json'))).json())['gles-gears']?.covered;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  new Filesystem(sys).addTar(t.work(tar));
  new CompositorWindows(sys, { canvas: document.getElementById('out') });
  const gl = new MesaGl(sys);
  await t.timeout(sys.boot(), 60000, 'boot');
  await t.timeout(gl.start(), 120000, 'Mesa');
  const r = await t.timeout(sys.run(['/probes/gles-gears', '30'], { env: { HOME: '/' } }), 120000, 'gles-gears');
  const got = (k) => new RegExp(`^${k} (.*)$`, 'm').exec(r.stdout)?.[1];
  t.say(`renderer ${got('renderer')}`);
  t.eq('exit', r.code, 0, r.stderr.trim().split('\n').slice(-2).join(' / '));
  t.eq('30 frames', got('frames'), '30');
  t.eq('no GL error', got('gl error'), '0');
  const covered = Number(got('covered')?.replace('%', ''));
  if (want == null) t.check(`it drew (${covered}%; no reference from the host's Mesa)`, covered > 0);
  else t.check(`it drew ${covered}% of the frame, as the host's Mesa did (${want}%)`, Math.abs(covered - want) <= 1);
  await sys.shutdown();
}
