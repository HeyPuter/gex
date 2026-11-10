// fresnel-gpui as a Wayland client of the page's
// compositor (no X: DISPLAY unset), its tree from WORK/fresnel-tree.tar.
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';
import { CompositorWindows } from '../../page/runtime/src/windows/compositor.js';
import { MesaGl } from '../../page/runtime/src/gl.js';

export const meta = { hosts: ['browser'], arches: ['x86'], images: [], timeout: 600, html: `<style>html, body { margin: 0; background: #000; }</style>
<canvas id="out" width="1280" height="760" style="width: 1280px; height: 760px; display: block;" tabindex="0"></canvas>` };

export default async function (t) {
  const { until, say } = t;
  if (!(await navigator.gpu?.requestAdapter().catch(() => null))) return t.skip('no WebGPU adapter here');
  for (const f of ['fresnel-tree.tar', 'fresnel-gpui', 'fresnel-daemon']) if (!(await fetch(t.work(f), { method: 'HEAD' })).ok) return t.skip(`no WORK/${f} (Fresnel's wasm32 build, debdemo)`);
  await t.cdp({ method: 'Emulation.setDeviceMetricsOverride', width: 1300, height: 780, deviceScaleFactor: 1, mobile: false });
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('fresnel-tree.tar'));
  fs.addFiles({
    '/usr/bin/fresnel-gpui': new URL(t.work('fresnel-gpui')),
    '/usr/bin/fresnel-daemon': new URL(t.work('fresnel-daemon')),
    '/usr/bin/fresnel': new URL(t.work('fresnel')),
  }, { mode: 0o755 });
  const canvas = document.getElementById('out');
  const out = new CompositorWindows(sys, { canvas, scale: 1 });
  new MesaGl(sys);
  await t.time('boot', () => t.timeout(sys.boot(), 120000, 'boot'));
  const env = { ...out.env, HOME: '/root', PATH: '/usr/bin:/bin', SHELL: '/bin/bash', TERM: 'xterm-256color' };
  delete env.DISPLAY;
  const id = await sys.run(['/usr/bin/id'], { env });
  say(`id: ${id.stdout.trim()}`);
  const p = sys.spawn(['/usr/bin/fresnel-gpui'], { env });
  const o = t.lines(p);
  const err = t.lines(p, p.stderr);
  const said = () => `stdout ${JSON.stringify(o.slice(-6))}, stderr ${JSON.stringify(err.slice(-10))}`;
  const t0 = performance.now();
  // The window: a light grey sidebar on the right of a 1100-wide window.
  let d;
  await until(async () => { d = await out.describe(); return d?.windows > 0 || d?.titles?.includes("Fresnel"); }, 'a window', 300000)
    .catch(async (e) => { await t.screenshot('fresnel-nowindow'); throw new Error(`${e.message}; ${JSON.stringify(d)}; ${said()}`); });
  say(`window after ${((performance.now() - t0) / 1000).toFixed(1)} s: ${JSON.stringify(d).slice(0, 400)}`);
  // Its shell's prompt: the session the client opens on start.
  await new Promise((r) => setTimeout(r, 30000));
  await t.screenshot('fresnel-window');
  const nw = await sys.run(['/usr/bin/fresnel', 'new'], { env });
  say(`fresnel new: ${nw.code} ${nw.stdout.trim()} ${nw.stderr.trim()}`);
  const lg = await sys.run(['/bin/sh', '-c', 'ls -la /tmp /tmp/fresnel* 2>&1; tail -n 40 /tmp/fresnel*/daemon.log /root/.local/state/fresnel/*.log 2>&1'], { env });
  say(`daemon: ${lg.stdout}`);
  await new Promise((r) => setTimeout(r, 15000));
  await t.screenshot('fresnel-after-new');
  const r = canvas.getBoundingClientRect();
  await t.click(Math.round(r.left + 400), Math.round(r.top + 300));
  await new Promise((r) => setTimeout(r, 2000));
  await t.type('echo hello');
  await new Promise((r) => setTimeout(r, 1000));
  await t.key('Enter', 'Enter');
  await new Promise((r) => setTimeout(r, 15000));
  await t.screenshot('fresnel-echo');
  say(said());
  t.check('the client is still running', p.code === undefined || p.code === null, said());
  await sys.shutdown();
}
