// The self-checking GL probes (guest/gl/probes) in the page: each an x86-64
// program on the guest GL (fixtures/images.mjs `gl-probes`), drawing into a
// pbuffer through /dev/gex-gl onto Mesa's gex_wgpu over WebGPU, and reading
// its pixels back. Each must exit 0 and end with the line the same probe
// printed on the host's own Mesa: the probe decides its cases, so its native
// run is the expectation. A probe whose native run did not pass (a check
// that is about gex, not GL) must exit 0. A probe needing more GL than this
// adapter gives (a software adapter has no dual-source blending, so GL 3.2)
// is left out, and said so.
import { System } from '../../web/runtime/src/index.js';
import { Filesystem } from '../../web/runtime/src/fs.js';
import { CompositorWindows } from '../../web/runtime/src/windows/compositor.js';
import { MesaGl } from '../../web/runtime/src/gl.js';

export const meta = { hosts: ['browser'], arches: ['x86'], images: ['gl-probes'], timeout: 1500, html: '<canvas id="out" width="640" height="480"></canvas>' };

export default async function (t) {
  if (!(await navigator.gpu?.requestAdapter().catch(() => null))) return t.skip('no WebGPU adapter here');
  const expect = await (await fetch(t.work('gl-probes.json'))).json();
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  new Filesystem(sys).addTar(t.work('gl-probes.tar'));
  new CompositorWindows(sys, { canvas: document.getElementById('out') });
  const gl = new MesaGl(sys);
  await t.timeout(sys.boot(), 60000, 'boot');
  t.say(`Mesa: ${await t.timeout(gl.start(), 120000, 'Mesa')}`);
  const v = await t.timeout(sys.run(['/probes/gl-version'], { env: { HOME: '/' } }), 60000, 'gl-version');
  const m = v.stdout.match(/^(\d+)\.(\d+)/);
  if (!t.check('GL version', v.code === 0 && m, `exit ${v.code}: ${v.stdout}${v.stderr}`)) return sys.shutdown();
  const have = Number(m[1]) * 100 + Number(m[2]) * 10;
  t.say(`GL ${m[1]}.${m[2]}`);
  const left = [];
  for (const [name, { want, needs }] of Object.entries(expect)) {
    if (needs > have) {
      left.push(`${name} (${needs})`);
      continue;
    }
    const r = await t.timeout(sys.run([`/probes/${name}`], { env: { HOME: '/' } }), 240000, name).catch((e) => ({ code: -1, stdout: '', stderr: String(e.message) }));
    const last = r.stdout.split('\n').filter((l) => l.trim()).pop() ?? '';
    const err = r.stderr.trim().split('\n').slice(-2).join(' / ');
    if (want === null) t.check(`${name}: exit 0 (no native reference)`, r.code === 0, `exit ${r.code}: ${last} ${err}`);
    else t.eq(`${name}`, [r.code, last], [0, want], err);
  }
  if (left.length) t.say(`left out, needing more than GL ${have}: ${left.join(', ')}`);
  await sys.shutdown();
}
