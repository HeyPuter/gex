// KDE Plasma through the package as a page with no eject has it: debdemo's
// KDE site (the `kde` image: its tree, lazy list and rest bundle) laid with
// Filesystem.addTar, the desktop drawn by CompositorWindows, and the
// session (gex-kde-session) spawned as the site's page starts it. Up when
// plasmashell says its startup completed.

export const meta = { hosts: ['browser'], arches: ['a64'], images: ['package', 'kde'], timeout: 900, html: `<style>html, body { margin: 0; }</style>
<canvas id="out" width="1280" height="800" style="width: 100vw; display: block;"></canvas>` };

export default async function (t) {
  const { say, check, timeout, until } = t;
  const adapter = await navigator.gpu?.requestAdapter().catch(() => null);
  if (!adapter) return t.skip('no WebGPU adapter here');
  // Not literals, which vite would try to resolve as its own.
  const pkg = t.work('pkg/');
  const { System } = await import(/* @vite-ignore */ `${pkg}index.mjs`);
  const { Filesystem } = await import(/* @vite-ignore */ `${pkg}fs.mjs`);
  const { CompositorWindows } = await import(/* @vite-ignore */ `${pkg}windows-compositor.mjs`);
  const sys = new System({ debug: t.debug() });
  const fs = new Filesystem(sys);
  fs.addTar(t.work('kde/kde.tar.zst'), { lazy: t.work('kde/lazy.json') });
  const out = new CompositorWindows(sys, { canvas: document.getElementById('out'), scale: 1, x11: 'rootful' });
  const t0 = performance.now();
  await timeout(sys.boot(), 180000, 'boot');
  say(`booted in ${(performance.now() - t0).toFixed(0)} ms`);
  const env = {
    PATH: '/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin', HOME: '/root', TERM: 'xterm-256color', LANG: 'C.UTF-8',
    ...out.env, XDG_RUNTIME_DIR: '/run/user/0', DISPLAY: ':0',
  };
  const p = sys.spawn(['/usr/bin/bash', '/usr/local/bin/gex-kde-session'], { env, cwd: '/root' });
  let log = '';
  const keep = (s) => new WritableStream({ write(b) { log = (log + new TextDecoder().decode(b)).slice(-200000); } });
  p.stdout.pipeTo(keep()).catch(() => {});
  p.stderr.pipeTo(keep()).catch(() => {});
  const up = await t.time('plasmashell up', () => until(() => /Plasma Shell startup completed/.test(log), 'plasmashell up', 600000))
    .then(() => true, () => false);
  check('plasmashell says its startup completed', up, `log ${JSON.stringify(log.slice(-1500))}; console ${JSON.stringify(sys.state.console.slice(-800))}`);
  const d = await out.describe();
  say(`scene: ${JSON.stringify(d).slice(0, 400)}`);
  await new Promise((r) => setTimeout(r, 5000));
  await t.screenshot('kde');
  await sys.shutdown();
}
