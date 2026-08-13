// Does virglrenderer come up on WebGL 2?
//
//   node web/test/virgl-webgl.mjs
//
// The renderer is compiled to wasm and pointed at a WebGL 2 context (GLES 3.0,
// which virglrenderer has a backend for). This loads it in headless Chromium
// and prints the capability set it would offer a guest -- the point is *what*:
// WebGL 2 has no compute, geometry/tessellation shaders or texture buffers, and
// the size of that reduction decides which programs can be served.
//
// Needs --enable-unsafe-swiftshader (software rasteriser), so nothing here is a
// performance measurement.
import { spawn } from 'node:child_process';
import { mkdtemp, writeFile, rm, copyFile, access } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';

const BUILD = process.env.GEX_VIRGL_WASM || join(tmpdir(), 'gex-virgl-wasm');
const CHROME = process.env.CHROME || '/usr/bin/chromium';

const exists = async (p) => access(p).then(() => true, () => false);

if (!(await exists(join(BUILD, 'gexvirgl.mjs')))) {
  console.log(`skip: no module at ${BUILD} (run web/build-virgl-wasm.sh)`);
  process.exit(0);
}
if (!(await exists(CHROME))) {
  console.log(`skip: no browser at ${CHROME}`);
  process.exit(0);
}

const dir = await mkdtemp(join(tmpdir(), 'gex-virgl-test-'));
await copyFile(join(BUILD, 'gexvirgl.mjs'), join(dir, 'gexvirgl.mjs'));
await copyFile(join(BUILD, 'gexvirgl.wasm'), join(dir, 'gexvirgl.wasm'));

// A recorded stream, if there is one. Capture it with:
//
//   GEX_VTEST_RECORD=gears.vtest gexc run ... glxgears
//
// It is every byte Mesa asked of a GPU while glxgears ran; replaying it asks
// whether WebGL 2 can execute what Mesa emits. No guest, rootfs or translator.
const STREAM = process.env.GEX_VTEST_STREAM || '';
let replay = false;
if (STREAM && (await exists(STREAM))) {
  await copyFile(STREAM, join(dir, 'stream.bin'));
  replay = true;
}

// The native renderer's answers to the same stream, from GEX_VTEST_REPLIES.
//
// The replies agree up to byte 32, then diverge on the capability set (5,508
// bytes native vs 308 here): the recording is what Mesa emits having been told
// it has GL 4.3, so replaying it into a smaller capability set asks for
// features never offered, and a blank frame here is expected, not a defect.
// This is a smoke test for transport, framing and dispatch; it cannot prove a
// picture, so the pixel count below is reported, never asserted.
const REPLIES = process.env.GEX_VTEST_REPLIES || '';
let compare = false;
if (replay && REPLIES && (await exists(REPLIES))) {
  await copyFile(REPLIES, join(dir, 'replies.bin'));
  compare = true;
}

// The result goes into the title, which --dump-dom brings back. Anything
// printed to the console would need a debugging protocol to collect.
await writeFile(
  join(dir, 'index.html'),
  `<!doctype html><meta charset=utf-8><title>pending</title>
<canvas id="gex-gl" width="300" height="300"></canvas>
<pre id="out">pending</pre><pre id="png"></pre>
<script type="module">
const REPLAY = __REPLAY__;
const COMPARE = __COMPARE__;
import Module from './gexvirgl.mjs';
const say = (s) => { document.getElementById('out').textContent = s; document.title = s; };
// virglrenderer explains itself on stderr; collect it or a failure is a number.
// Deduplicated: a per-draw error would otherwise bury the one-time cause at the
// tail of the log.
const log = [];
const seen = new Set();
const note = (t) => { const k = String(t).slice(0, 120); if (!seen.has(k)) { seen.add(k); log.push(k); } };
try {
  const m = await Module({ printErr: note, print: note });
  const rc = m._gex_virgl_init(300, 300);
  if (rc !== 0) { say('INIT FAILED ' + rc + ' :: ' + log.join(' / ')); }
  else if (!REPLAY) { say('OK ' + m.UTF8ToString(m._gex_virgl_describe()) + ' :: ' + log.join(' / ')); }
  else {
    const stream = new Uint8Array(await (await fetch('./stream.bin')).arrayBuffer());
    // In pieces, and deliberately not on command boundaries: the guest's
    // socket delivers whatever it has, so a command arriving in two halves is
    // the normal case and the rewind that handles it needs exercising.
    const want = COMPARE
      ? new Uint8Array(await (await fetch('./replies.bin')).arrayBuffer())
      : null;
    let at = 0, diffAt = -1, diffCmd = -1;
    const CHUNK = 4093;
    let ran = 0, replies = 0, fed = 0, refused = false;
    let biggest = new Uint8Array(0);
    const buf = m._malloc(CHUNK);
    for (let off = 0; off < stream.length && !refused; off += CHUNK) {
      const part = stream.subarray(off, Math.min(off + CHUNK, stream.length));
      m.HEAPU8.set(part, buf);
      m._gex_virgl_feed(buf, part.length);
      fed += part.length;
      const n = m._gex_virgl_pump();
      if (n < 0) { refused = true; break; }
      ran += n;
      const got = m._gex_virgl_pending();
      if (want && diffAt < 0 && got) {
        const mine = m.HEAPU8.subarray(m._gex_virgl_replies(),
                                       m._gex_virgl_replies() + got);
        for (let i = 0; i < mine.length && at + i < want.length; i++) {
          if (mine[i] !== want[at + i]) { diffAt = at + i; diffCmd = ran; break; }
        }
        at += got;
      }
      // Keep the biggest reply: a TRANSFER_GET of the frame (300x300 RGBA is
      // 360,000 bytes) dwarfs a fence or capability answer, so the largest is
      // the picture -- the only thing that says whether it drew.
      if (got > biggest.length) {
        biggest = m.HEAPU8.slice(m._gex_virgl_replies(), m._gex_virgl_replies() + got);
      }
      replies += got;
      m._gex_virgl_drain();
    }
    m._free(buf);
    const tail = log.join(' | ');
    if (refused) {
      say('PUMP REFUSED after ' + ran + ' commands, ' + fed + ' of '
          + stream.length + ' bytes :: ' + tail);
    } else {
      // The frame as a PNG, so it can be looked at. Counted across the whole
      // reply: a pump can answer two TRANSFER_GETs at once, so a fixed offset
      // is a guess that reads zeros for the wrong reason.
      let nz = 0;
      for (let i = 0; i < biggest.length; i++) if (biggest[i]) nz++;
      let drawn = 'biggest reply ' + biggest.length + 'B, ' + nz + ' non-zero bytes';
      const W = 300, H = 300;
      if (biggest.length >= W * H * 4) {
        const px = biggest.subarray(biggest.length - W * H * 4);
        const c = document.createElement('canvas');
        c.width = W; c.height = H;
        const cx = c.getContext('2d');
        const img = cx.createImageData(W, H);
        // Bottom-up: GL's origin is the lower left, a canvas's is upper left.
        for (let y = 0; y < H; y++) {
          const src = (H - 1 - y) * W * 4;
          img.data.set(px.subarray(src, src + W * 4), y * W * 4);
        }
        cx.putImageData(img, 0, 0);
        document.getElementById('png').textContent = c.toDataURL('image/png');
        let nonzero = 0;
        for (let i = 0; i < px.length; i += 4) if (px[i] || px[i+1] || px[i+2]) nonzero++;
        drawn += ', ' + nonzero + '/' + (W * H) + ' pixels non-black in the tail';
      }
      let verdict = '';
      if (want) {
        verdict = diffAt < 0
          ? ', replies match the native renderer for ' + Math.min(at, want.length) + ' bytes'
          : ', FIRST DIVERGENCE at reply byte ' + diffAt + ' after ' + diffCmd
            + ' commands (mine ' + Math.min(at, want.length) + 'B vs native '
            + want.length + 'B)';
      }
      say('OK replayed ' + stream.length + ' bytes, ' + ran + ' commands, '
          + replies + ' reply bytes, ' + drawn + verdict
          + ' :: ' + m.UTF8ToString(m._gex_virgl_gl_state())
          + ' :: ' + tail);
    }
  }
} catch (e) {
  say('THREW ' + (e && e.message ? e.message : String(e)) + ' :: ' + log.join(' / '));
}
</script>`.replace('__REPLAY__', String(replay)).replace('__COMPARE__', String(compare)),
);

// A file:// page cannot load an ES module sibling under Chromium's rules, so
// serve the directory.
const server = createServer(async (req, res) => {
  const name = (req.url === '/' ? '/index.html' : req.url).split('?')[0];
  try {
    const body = await readFile(join(dir, name));
    const type = name.endsWith('.wasm')
      ? 'application/wasm'
      : name.endsWith('.mjs')
        ? 'text/javascript'
        : 'text/html';
    res.writeHead(200, { 'content-type': type });
    res.end(body);
  } catch {
    res.writeHead(404).end();
  }
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const url = `http://127.0.0.1:${server.address().port}/`;

const dom = await new Promise((resolve, reject) => {
  const args = [
    '--headless',
    '--disable-gpu-sandbox',
    '--no-sandbox',
    '--enable-unsafe-swiftshader',
    '--use-gl=swiftshader',
    '--virtual-time-budget=20000',
    '--dump-dom',
    url,
  ];
  const p = spawn(CHROME, args, { stdio: ['ignore', 'pipe', 'pipe'] });
  let out = '';
  let err = '';
  p.stdout.on('data', (d) => (out += d));
  p.stderr.on('data', (d) => (err += d));
  p.on('error', reject);
  p.on('close', () => resolve({ out, err }));
});
server.close();

const m = /<pre id="out">([^<]*)<\/pre>/.exec(dom.out);
const said = m ? m[1] : '(no result element)';
const png = /<pre id="png">data:image\/png;base64,([^<]*)<\/pre>/.exec(dom.out);
if (png && process.env.GEX_VTEST_PNG) {
  await writeFile(process.env.GEX_VTEST_PNG, Buffer.from(png[1], 'base64'));
  console.log(`     wrote ${process.env.GEX_VTEST_PNG}`);
}
await rm(dir, { recursive: true, force: true });

console.log(`     ${said}`);
if (said.startsWith('OK ')) {
  console.log(
    replay
      ? 'ok   virgl-webgl (transport and dispatch; a replay cannot prove pixels)'
      : 'ok   virgl-webgl',
  );
  process.exit(0);
}
console.log('FAIL virgl-webgl: the renderer did not come up on WebGL 2');
console.log(
  dom.err
    .split('\n')
    .filter((l) => l && !l.includes('Fontconfig'))
    .slice(-12)
    .map((l) => `     ${l}`)
    .join('\n'),
);
process.exit(1);
