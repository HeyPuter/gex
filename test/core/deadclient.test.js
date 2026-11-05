// A client's windows go when it does, however it goes: fixtures/programs/
// deadclient.c (the busybox image's rt-deadclient, wasm32) maps four windows
// from four processes; then one child is killed (SIGKILL), one traps (a wasm
// trap: its Worker throws rather than exits) and one closes its connection
// and stays up. The compositor's windows (describe().titles) are taken at
// each phase line: all four shown, and afterwards only the live parent's,
// with both dead children reaped. SwiftShader here; on a GPU through bench.sh.
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';
import { CompositorWindows } from '../../page/runtime/src/windows/compositor.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 300, html: '<canvas id="out" width="1280" height="720"></canvas>' };

const ALL = ['dc-alive', 'dc-killed', 'dc-trap', 'dc-hangup'];

export default async function (t) {
  const { check, timeout, until, say } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  new MemoryFilesystem(sys).addTar(t.work('bb.tar.zst'));
  const out = new CompositorWindows(sys, { canvas: document.getElementById('out') });
  await timeout(sys.boot(), 60000, 'boot');
  const p = sys.spawn(['rt-deadclient'], { env: { PATH: '/bin', HOME: '/root', ...out.env } });
  const marks = {};
  let text = '';
  const read = async (s) => {
    const dec = new TextDecoder();
    for (const r = s.getReader(); ;) {
      const { value, done } = await r.read();
      if (done) return;
      const chunk = dec.decode(value, { stream: true });
      text += chunk;
      for (const m of chunk.matchAll(/^phase (\w+)$/gm)) out.describe().then((d) => { marks[m[1]] = { titles: d.titles, log: d.log }; });
    }
  };
  const reading = Promise.all([read(p.stdout), read(p.stderr)]);
  await until(() => text.includes('deadclient done') && marks.after, 'the clients ran to the end', 240000)
    .catch((e) => { throw new Error(`${e.message}: ${text.slice(-400)}`); });
  for (const [k, v] of Object.entries(marks)) say(`  ${k}: ${JSON.stringify(v.titles)}`);
  check(`all four windows shown (${JSON.stringify(marks.shown?.titles)})`, ALL.every((w) => marks.shown?.titles?.includes(w)));
  check('the killed child was reaped, killed by SIGKILL', /^dc-killed status 0x9$/m.test(text), text.slice(-300));
  check('the trapped child was reaped', /^dc-trap status /m.test(text), text.slice(-300));
  const after = marks.after.titles ?? [];
  const log = (marks.after.log ?? []).slice(-8).join(' / ');
  check(`the live client's window stays (${JSON.stringify(after)})`, after.includes('dc-alive'), log);
  check("a killed client's window goes", !after.includes('dc-killed'), log);
  check("a trapped client's window goes", !after.includes('dc-trap'), log);
  check("a client's window goes when it closes its connection", !after.includes('dc-hangup'), log);
  p.kill('SIGKILL');
  await sys.shutdown();
  await reading.catch(() => {});
}
