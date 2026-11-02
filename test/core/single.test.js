// SingleCanvas: the windows laid out in one element of the page, unframed;
// and an X client's beside a Wayland one's.
import { System } from '../../web/runtime/src/index.js';
import { Filesystem } from '../../web/runtime/src/fs.js';
import { SingleCanvas } from '../../web/runtime/src/windows/single.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 120, html: `<style>html, body { margin: 0; } #app { position: relative; width: 640px; height: 480px; }</style>
<div id="app"></div>` };

export default async function (t) {
  const { check, eq, timeout, until } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new Filesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  const app = document.getElementById('app');
  const out = new SingleCanvas(sys, { container: app });
  await timeout(sys.boot(), 60000, 'boot');
  const env = { PATH: '/bin', ...out.env };
  const p = sys.spawn(['rt-wl', 'single'], { env });
  const q = sys.spawn(['rt-x11', 'xsingle'], { env });
  const d = await until(async () => {
    const v = await out.describe();
    return v?.titles?.includes('single') && v.titles.includes('xsingle') && v.lit > 0 ? v : null;
  }, 'both windows, drawn', 60000).catch((e) => { throw new Error(`${e.message}: ${JSON.stringify(sys.state.console.slice(-400))}`); });
  eq('windows', d.windows, 2);
  eq('in the element', app.querySelectorAll('canvas').length >= 2, true);
  check('unframed: a canvas the window\'s size', [...app.querySelectorAll('canvas')].some((c) => c.width === 200 && c.height === 100));
  p.kill();
  q.kill();
  await p.exited;
  await q.exited;
  await until(async () => (await out.describe())?.windows === 0, 'the windows gone');
  check('gone with their programs', true);
  await sys.shutdown();
}
