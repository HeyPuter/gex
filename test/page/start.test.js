// The eject library's start() (page/src/app.js, an ejected build's app.mjs)
// on the runtime API: a bundle (busybox's tree) and its program, run as the
// first spawn of a system with the kernel's own pid 1, its windows laid out
// in the page (SingleCanvas: an X client's window, the X server started at
// its first connection), its output, its exit status, and kill().
import { start } from '../../page/src/app.js';

export const meta = {
  hosts: ['browser'], images: ['busybox'], timeout: 180,
  html: '<div id="app" style="position:relative;width:640px;height:480px"></div>',
};

export default async function (t) {
  const { say, check, eq, until, timeout } = t;
  const { arch, ...switches } = t.debug();
  const zstd = new URL('../vendor/zstddec/zstddec-stream.mjs', new URL(t.baseUrl, location.href)).href;
  const build = (argv) => ({
    arch: arch ?? 'aarch64',
    tar: [t.work('bb.tar')],
    zstd,
    exe: '/bin/sh',
    argv,
    env: ['PATH=/bin:/usr/bin', 'HOME=/root', 'DISPLAY=:0'],
    cwd: '/',
    featuresOff: [],
    compositor: false,
    jitFallback: true,
    gl: false,
    hostx: true,
    switches,
  });
  const t0 = performance.now();
  const app = await timeout(start({
    container: document.getElementById('app'),
    filesystem: 'memory',
    size: { w: 640, h: 480 },
    baseUrl: t.baseUrl,
    build: build(['-c', 'echo "ppid $PPID"; cat /proc/1/comm; rt-x11 demo & sleep 600']),
  }), 60000, 'start');
  say(`started in ${(performance.now() - t0).toFixed(0)} ms`);
  await until(async () => (await app.describe())?.titles?.includes('demo'), 'the X window', 60000)
    .catch((e) => { throw new Error(`${e.message}: ${app.state.out.slice(-800)}`); });
  check('an X client\'s window in the page', true);
  await until(() => /^init$/m.test(app.state.out), 'the output', 20000).catch(() => {});
  const out = app.state.out.replace(/\r/g, '');
  check('the program is a child of the kernel\'s pid 1', /^ppid 1$/m.test(out) && /^init$/m.test(out), out.slice(0, 300));
  await until(() => ['first-frame', 'ready'].includes(app.state.progress.phase), 'the first frame', 20000).catch(() => {});
  check('the first frame said', ['first-frame', 'ready'].includes(app.state.progress.phase), app.state.progress.phase);
  app.kill();
  eq('kill(): exited 137', await timeout(app.exited, 20000, 'kill'), 137);
}
