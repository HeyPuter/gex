// KDE Plasma through the package as a page with no eject has it: debdemo's
// KDE site (the `kde` image: its tree, lazy list and rest bundle) laid with
// Filesystem.addTar, the desktop drawn by CompositorWindows, and the
// session (gex-kde-session) spawned as the site's page starts it. Up when
// plasmashell says its startup completed.

// Alone: a desktop's dozens of processes, each a Worker with its own memory,
// beside other tests' pages in the same renderer ran out of address space.
export const meta = { hosts: ['browser'], arches: ['a64'], images: ['package', 'kde'], timeout: 1200, alone: true,
  html: `<style>html, body { margin: 0; }</style>
<canvas id="out" width="1280" height="800" style="width: 100vw; display: block;"></canvas>` };

export default async function (t) {
  const { say, check, timeout, until } = t;
  const adapter = await navigator.gpu?.requestAdapter().catch(() => null);
  if (!adapter) return t.skip('no WebGPU adapter here');
  // Not literals, which vite would try to resolve as its own.
  const pkg = t.work('pkg/');
  const { System } = await import(/* @vite-ignore */ `${pkg}index.mjs`);
  const { MemoryFilesystem } = await import(/* @vite-ignore */ `${pkg}fs.mjs`);
  const { CompositorWindows } = await import(/* @vite-ignore */ `${pkg}windows-compositor.mjs`);
  const sys = new System({ debug: t.debug() });
  const fs = new MemoryFilesystem(sys);
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
  // The session's output, each chunk stamped with the seconds since the spawn.
  let log = '', stamped = '';
  const t1 = performance.now();
  const keep = (s) => new WritableStream({ write(b) {
    const text = new TextDecoder().decode(b);
    log = (log + text).slice(-200000);
    stamped += text.replace(/^(?=.)/gm, `[${((performance.now() - t1) / 1000).toFixed(1)}] `);
  } });
  p.stdout.pipeTo(keep()).catch(() => {});
  p.stderr.pipeTo(keep()).catch(() => {});
  // Up in about 70-100 s alone on an idle host; 440 s beside 48 busy loops
  // on 24 threads.
  const up = await t.time('plasmashell up', () => until(() => /Plasma Shell startup completed/.test(log), 'plasmashell up', 900000))
    .then(() => true, () => false);
  if (!up) {
    // Where it stopped: the session's whole output, the system's console,
    // and what was running then.
    const ps = await timeout(sys.run(['ps', '-eo', 'pid,ppid,stat,time,etime,args'], { env }), 30000, 'ps')
      .then((r) => r.stdout + r.stderr, (e) => `ps: ${e?.message ?? e}`);
    await t.saveFile('session.log', stamped);
    await t.saveFile('console.log', sys.state.console);
    await t.saveFile('ps.txt', ps);
  }
  check('plasmashell says its startup completed', up, `log ${JSON.stringify(log.slice(-1500))}; console ${JSON.stringify(sys.state.console.slice(-800))}`);
  const d = await out.describe();
  say(`scene: ${JSON.stringify(d).slice(0, 400)}`);
  await new Promise((r) => setTimeout(r, 5000));
  await t.screenshot('kde');
  // The renderer's memory with the desktop up (KiB), for a run's budget.
  const mem = await t.procStatus();
  if (mem) say(`renderer: VmData ${(mem.data / 1024).toFixed(0)} MiB, VmRSS ${(mem.rss / 1024).toFixed(0)} MiB, VmHWM ${(mem.hwm / 1024).toFixed(0)} MiB`);
  await sys.shutdown();
}
