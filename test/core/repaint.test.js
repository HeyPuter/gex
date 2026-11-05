// Repaints reach the screen only when they change it, and carry only their
// damage: a client that commits the same pixels over and over draws no frame
// and uploads nothing (a canvas presented for each such commit costs a full
// frame a time), and a narrow damage rectangle is uploaded as its own pixels,
// not as the rows of the buffer it crosses (those of an 8-pixel column of a
// 1280-pixel buffer are 160 times its pixels). The client is
// fixtures/programs/repaint.c (the busybox image's rt-repaint): one 8x400
// damage rectangle committed ROUNDS times with its pixels unchanged, then
// ROUNDS times changed; the compositor's counters (describe().frames,
// .uploads) are taken at each phase line it prints, once they have settled:
// the client waits for the test's cue to start each phase (--wait). Also: the still output
// reads back, the same twice. SwiftShader here; on a GPU through bench.sh.
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';
import { CompositorWindows } from '../../page/runtime/src/windows/compositor.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 300, html: '<canvas id="out" width="1280" height="720"></canvas>' };

const ROUNDS = 60;
const RECT = 8 * 400 * 4;

export default async function (t) {
  const { check, timeout, until, say } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  new MemoryFilesystem(sys).addTar(t.work('bb.tar.zst'));
  const out = new CompositorWindows(sys, { canvas: document.getElementById('out') });
  await timeout(sys.boot(), 60000, 'boot');
  const p = sys.spawn(['rt-repaint', String(ROUNDS), '--wait'], { env: { PATH: '/bin', HOME: '/root', ...out.env } });
  const marks = {};
  let text = '';
  const stdin = p.stdin.getWriter();
  // The counters once no frame has been drawn for half a second.
  const settled = async () => {
    let d = await out.describe();
    for (let quiet = 0; quiet < 5;) {
      await new Promise((r) => setTimeout(r, 100));
      const e = await out.describe();
      quiet = e.frames === d.frames ? quiet + 1 : 0;
      d = e;
    }
    return { frames: d.frames, uploads: d.uploads };
  };
  const reading = (async () => {
    const dec = new TextDecoder();
    for (const r = p.stdout.getReader(); ;) {
      const { value, done } = await r.read();
      if (done) return;
      const chunk = dec.decode(value, { stream: true });
      text += chunk;
      for (const m of chunk.matchAll(/^phase (\w+) (ready|end)$/gm)) {
        const key = `${m[1]} ${m[2] === 'ready' ? 'start' : 'end'}`;
        marks[key] = await settled();
        if (m[2] === 'ready') await stdin.write(new TextEncoder().encode('go\n'));
      }
    }
  })();
  await until(() => text.includes('repaint done') && marks['change end'], 'the client ran both phases', 240000)
    .catch((e) => { throw new Error(`${e.message}: ${text.slice(-400)}`); });
  for (const [k, v] of Object.entries(marks)) say(`  ${k}: frames ${v.frames}, uploads ${JSON.stringify(v.uploads)}`);
  const delta = (a, b) => ({
    frames: marks[b].frames - marks[a].frames,
    n: marks[b].uploads.n - marks[a].uploads.n,
    bytes: marks[b].uploads.bytes - marks[a].uploads.bytes,
    same: marks[b].uploads.same - marks[a].uploads.same,
    // A commit the compositor did not see on its own (two between frames)
    // is a whole-buffer upload: counted apart.
    full: marks[b].uploads.full - marks[a].uploads.full,
    fullBytes: marks[b].uploads.fullBytes - marks[a].uploads.fullBytes,
  });
  const same = delta('same start', 'same end');
  const change = delta('change start', 'change end');
  check(`unchanged damage draws no frame (${same.frames} in ${ROUNDS} commits)`, same.frames === 0);
  check(`unchanged damage uploads nothing (${same.n} uploads, ${same.same} left out)`, same.n === 0 && same.same >= ROUNDS / 2);
  check(`changed damage draws one frame a commit at most, one per two at least (${change.frames} in ${ROUNDS})`, change.frames >= ROUNDS / 2 && change.frames <= ROUNDS);
  check(`an upload carries the rectangle, not the rows (${change.bytes - change.fullBytes} bytes in ${change.n - change.full} uploads; the rectangle is ${RECT}, ${change.full} whole-buffer uploads apart)`,
    change.n > change.full && change.bytes - change.fullBytes <= (change.n - change.full) * RECT);
  // The output can be read on a still screen: on a WebGPU canvas, drawn
  // straight into the canvas's texture, the first read draws the scene again
  // for itself.
  const a = await out.describe();
  const b = await out.describe();
  check(`the still output reads back, the same twice (lit ${a?.lit}/${b?.lit})`, a?.lit > 0 && b?.lit === a.lit && b.hash === a.hash);
  p.kill('SIGKILL');
  await sys.shutdown();
  await reading.catch(() => {});
}
