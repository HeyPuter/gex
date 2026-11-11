// debdemo's debian-slim as it ships: `apt-get install fresnel` from the
// repository the page serves (RepoNetwork over WORK/slim/repo, the test's
// static server, which answers unknown paths with an HTML page), then
// fresnel-gpui as a Wayland client of the page's compositor. It finds its
// libraries only through the package's Depends: the Wayland ones it loads
// with dlopen, which no needed-library scan sees.
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';
import { CompositorWindows } from '../../page/runtime/src/windows/compositor.js';
import { MesaGl } from '../../page/runtime/src/gl.js';
import { RepoNetwork } from '../../page/runtime/examples/repo-network.mjs';

export const meta = { hosts: ['browser'], arches: ['x86'], images: ['slim'], timeout: 900, html: `<style>html, body { margin: 0; background: #000; }</style>
<canvas id="out" width="1280" height="760" style="width: 1280px; height: 760px; display: block;" tabindex="0"></canvas>` };

export default async function (t) {
  const { until, say, eq, check } = t;
  if (!(await navigator.gpu?.requestAdapter().catch(() => null))) return t.skip('no WebGPU adapter here');
  await t.cdp({ method: 'Emulation.setDeviceMetricsOverride', width: 1300, height: 780, deviceScaleFactor: 1, mobile: false });
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('slim/debian-slim.tar.zst'));
  sys.network = new RepoNetwork({ base: new URL(t.work('slim/repo/')) });
  const canvas = document.getElementById('out');
  const out = new CompositorWindows(sys, { canvas, scale: 1 });
  new MesaGl(sys);
  await t.time('boot', () => t.timeout(sys.boot(), 180000, 'boot'));
  const base = { PATH: '/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin', HOME: '/root', DEBIAN_FRONTEND: 'noninteractive' };
  const run = async (argv, ms = 600000) => {
    const r = await t.timeout(sys.run(argv, { env: base }), ms, argv.join(' '));
    if (r.code) say(`${argv.join(' ')}: exit ${r.code}: ${(r.stderr || r.stdout).slice(-800)}`);
    return r;
  };
  // Offline: Debian's archive is not reached here.
  await run(['rm', '-f', '/etc/apt/sources.list.d/debian.sources']);
  eq('apt-get update', (await run(['apt-get', 'update'])).code, 0);
  eq('apt-get install fresnel', (await t.time('install', () => run(['apt-get', 'install', '-y', 'fresnel']))).code, 0);
  const env = { ...out.env, ...base, SHELL: '/bin/bash', TERM: 'xterm-256color' };
  delete env.DISPLAY;
  const p = sys.spawn(['/usr/bin/fresnel-gpui'], { env });
  const o = t.lines(p);
  const err = t.lines(p, p.stderr);
  const said = () => `stdout ${JSON.stringify(o.slice(-6))}, stderr ${JSON.stringify(err.slice(-10))}`;
  let d;
  await until(async () => { d = await out.describe(); return d?.windows > 0 || (p.code !== undefined && p.code !== null); }, 'a window', 300000)
    .catch(async (e) => { await t.screenshot('slim-fresnel-nowindow'); throw new Error(`${e.message}; ${JSON.stringify(d)}; ${said()}`); });
  check('fresnel-gpui opened a window', d?.windows > 0, `${JSON.stringify(d)}; exit ${p.code}; ${said()}`);
  check('no missing Wayland library', !err.some((l) => /NoWaylandLib/.test(l)), said());
  await new Promise((r) => setTimeout(r, 20000));
  await t.screenshot('slim-fresnel');
  check('the client is still running', p.code === undefined || p.code === null, said());
  await sys.shutdown();
}
