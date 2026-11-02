// Clipboard: the page's text offered to a program's window, a program's copy
// coming out to the page, and one program's copy reaching another's paste.
import { System } from '../../web/runtime/src/index.js';
import { Filesystem } from '../../web/runtime/src/fs.js';
import { HostWindows } from '../../web/runtime/src/windows/host.js';
import { Clipboard } from '../../web/runtime/src/input.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 120, html: `<style>html, body { margin: 0; } #desk { position: relative; width: 800px; height: 600px; }</style>
<div id="desk"></div>` };

export default async function (t) {
  const { check, eq, timeout, until, lines, click } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new Filesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  const hw = new HostWindows(sys, { width: 800, height: 600, scale: 1 });
  const desk = document.getElementById('desk');
  const wins = [];
  let left = 0;
  hw.addEventListener('window', (e) => {
    const w = e.window;
    wins.push(w);
    const x = left;
    left += 250;
    Object.assign(w.canvas.style, { position: 'absolute', left: `${x}px`, top: '0px' });
    desk.append(w.canvas);
    w.place(x, 0);
  });
  // Without sync: this page's own clipboard stays out of it.
  const clip = new Clipboard(sys, { sync: false });
  const copies = [];
  clip.addEventListener('copy', (e) => copies.push(e.text));
  check('a second throws', (() => { try { new Clipboard(sys); return false; } catch { return true; } })());
  await timeout(sys.boot(), 60000, 'boot');
  const env = { PATH: '/bin', ...hw.env };

  const reader = sys.spawn(['rt-wl', 'reader', 'paste'], { env });
  const rOut = lines(reader);
  await until(() => rOut.includes('up'), 'the reader');
  clip.setText('from the page ✓');
  await until(() => rOut.includes('paste from the page ✓'), 'the page\'s text in a program', 10000)
    .catch((e) => { throw new Error(`${e.message}: ${JSON.stringify(rOut)}`); });
  check('the page\'s text reached the program', true);

  const writer = sys.spawn(['rt-wl', 'writer', 'copy', 'from the guest'], { env });
  const wOut = lines(writer);
  await until(() => wOut.includes('up') && wins.find((w) => w.title === 'writer'), 'the writer');
  // The keyboard, with a click: the program copies when it gets it.
  await click(wins.find((w) => w.title === 'writer').x + 20, 20);
  await until(() => copies.length, 'the program\'s copy', 10000)
    .catch((e) => { throw new Error(`${e.message}: ${JSON.stringify(wOut)}`); });
  eq('a program\'s copy comes out', copies, ['from the guest']);
  eq('the clipboard says so', clip.text, 'from the guest');
  // The other program, once it is busy again (a click), is offered it.
  await click(wins.find((w) => w.title === 'reader').x + 20, 20);
  await until(() => rOut.includes('paste from the guest'), 'one program\'s copy in another', 10000)
    .catch((e) => { throw new Error(`${e.message}: ${JSON.stringify(rOut)}`); });
  check('one program\'s copy reached another', true);
  reader.kill();
  writer.kill();
  await Promise.all([reader.exited, writer.exited]);
  await sys.shutdown();
}
