// Zed as a Wayland client of the page's compositor (no
// X: DISPLAY unset), its tree from WORK/fresnel-tree.tar, a file to edit.
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';
import { CompositorWindows } from '../../page/runtime/src/windows/compositor.js';
import { MesaGl } from '../../page/runtime/src/gl.js';

export const meta = { hosts: ['browser'], arches: ['x86'], images: [], timeout: 900, html: `<style>html, body { margin: 0; background: #000; }</style>
<canvas id="out" width="1540" height="1100" style="width: 1540px; height: 1100px; display: block;" tabindex="0"></canvas>` };

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export default async function (t) {
  const { until, say } = t;
  if (!(await navigator.gpu?.requestAdapter().catch(() => null))) return t.skip('no WebGPU adapter here');
  for (const f of ['fresnel-tree.tar', 'zed']) if (!(await fetch(t.work(f), { method: 'HEAD' })).ok) return t.skip(`no WORK/${f} (Zed's wasm32 build, debdemo)`);
  await t.cdp({ method: 'Emulation.setDeviceMetricsOverride', width: 1560, height: 1120, deviceScaleFactor: 1, mobile: false });
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('fresnel-tree.tar'));
  fs.addFiles({ '/usr/bin/zed': new URL(t.work('zed')) }, { mode: 0o755 });
  fs.addFiles({ '/work/hello.rs': 'fn main() {\n    println!("hi");\n}\n' });
  const canvas = document.getElementById('out');
  const out = new CompositorWindows(sys, { canvas, scale: 1 });
  new MesaGl(sys);
  await t.time('boot', () => t.timeout(sys.boot(), 120000, 'boot'));
  const env = { ...out.env, HOME: '/root', PATH: '/usr/bin:/bin', SHELL: '/bin/bash', TERM: 'xterm-256color', ZED_ALLOW_EMULATED_GPU: '1', ZED_ALLOW_ROOT: 'true', ZED_LOG: 'info' };
  delete env.DISPLAY;
  const p = sys.spawn(['/usr/bin/zed', '/work/hello.rs'], { env });
  const o = t.lines(p);
  const err = t.lines(p, p.stderr);
  const said = () => `stdout ${JSON.stringify(o.slice(-6))}, stderr ${JSON.stringify(err.slice(-12))}`;
  const t0 = performance.now();
  let d;
  await until(async () => { d = await out.describe(); return d?.windows > 0; }, 'a window', 600000)
    .catch(async (e) => { await t.screenshot('zed-nowindow'); throw new Error(`${e.message}; ${JSON.stringify(d)}; ${said()}`); });
  say(`window after ${((performance.now() - t0) / 1000).toFixed(1)} s: ${JSON.stringify(d).slice(0, 600)}`);
  await sleep(60000);
  await t.screenshot('zed-window');
  const r = canvas.getBoundingClientRect();
  // the trust dialog's "Stay in Restricted Mode", then line 4 (page coordinates: the frame's title bar is above the client)
  await t.click(Math.round(r.left + 735), Math.round(r.top + 395));
  await sleep(5000);
  await t.click(Math.round(r.left + 400), Math.round(r.top + 224));
  await sleep(3000);
  await t.type('hello from the page');
  await sleep(10000);
  await t.screenshot('zed-typed');
  const lg = await sys.run(['/bin/sh', '-c', 'tail -n 30 /root/.local/share/zed/logs/Zed.log 2>&1'], { env });
  say(`Zed.log: ${lg.stdout}`);
  say(said());
  t.check('Zed is still running', p.code === undefined || p.code === null, said());
  await sys.shutdown();
}
