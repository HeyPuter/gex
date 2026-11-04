// The eject library's start() (page/src/app.js) on the runtime API, a program
// that ends: its standard output and error in `state.out` and onLog, and
// its exit status from `exited` and onExit.
import { start } from '../../page/src/app.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 120, html: '<div id="app" style="position:relative;width:320px;height:240px"></div>' };

export default async function (t) {
  const { check, eq, timeout } = t;
  const { arch, ...switches } = t.debug();
  let logged = '';
  let exitSaid = null;
  const app = await timeout(start({
    container: document.getElementById('app'),
    filesystem: 'memory',
    size: { w: 320, h: 240 },
    baseUrl: t.baseUrl,
    onLog: (s) => { logged += s; },
    onExit: (st) => { exitSaid = st; },
    build: {
      arch: arch ?? 'aarch64', tar: [t.work('bb.tar')], zstd: new URL('../vendor/zstddec/zstddec-stream.mjs', new URL(t.baseUrl, location.href)).href,
      exe: '/bin/sh', argv: ['-c', 'echo to-stdout; echo to-stderr >&2; exit 5'], env: ['PATH=/bin'], cwd: '/',
      featuresOff: [], compositor: false, jitFallback: true, gl: false, switches,
    },
  }), 60000, 'start');
  eq('exited', await timeout(app.exited, 60000, 'exit'), 5);
  eq('onExit', exitSaid, 5);
  check('stdout in state.out', /to-stdout/.test(app.state.out), app.state.out.slice(-300));
  check('stderr in state.out', /to-stderr/.test(app.state.out));
  check('onLog has it too', /to-stdout/.test(logged) && /to-stderr/.test(logged));
}
