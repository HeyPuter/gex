// Does virglrenderer come up on WebGL 2?
//
//   node web/test/virgl-webgl.mjs
//
// The native path is settled: unmodified Debian glxgears renders through the
// host's libvirglrenderer on real OpenGL. The page cannot use that renderer,
// so the renderer itself is compiled to wasm and pointed at a WebGL 2 context
// instead -- WebGL 2 being GLES 3.0, and virglrenderer having a GLES backend
// because that is how it runs on phones.
//
// Whether that actually stands up is a question about a browser, so this asks
// one. It loads the module in headless Chromium, initialises the renderer,
// and prints the capability set it would offer a guest. The interesting part
// is not that it returns something: it is *what*. WebGL 2 has no compute, no
// geometry or tessellation shaders and no texture buffers, so the capability
// set is a reduced one, and the size of the reduction decides which programs
// can be served at all.
//
// Chromium needs --enable-unsafe-swiftshader for WebGL without a GPU. That is
// a software rasteriser, so nothing here is a performance measurement.
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
// It is every byte unmodified Mesa asked of a GPU while glxgears ran, so
// replaying it here asks the only question that matters about the port --
// not whether the renderer starts, but whether WebGL 2 can execute what Mesa
// actually emits. No guest, no rootfs and no translator are involved.
const STREAM = process.env.GEX_VTEST_STREAM || '';
let replay = false;
if (STREAM && (await exists(STREAM))) {
  await copyFile(STREAM, join(dir, 'stream.bin'));
  replay = true;
}

// The native renderer's answers to the same stream, from GEX_VTEST_REPLIES.
//
// This compares, and it is worth being clear about how far it can mean
// anything. The replies agree exactly up to reply byte 32, and then diverge
// on the capability set -- 5,508 bytes of it natively against 308 here,
// because that host is desktop GL 4.3 and this one is GLES 3.0.
//
// That divergence is correct. What it costs is the rest of the comparison,
// and more than the comparison. The recorded stream is what the guest's Mesa
// emits *having been told it has GL 4.3*: every shader, format and draw after
// the capability exchange was chosen on that basis. Replaying it into a
// renderer with a smaller capability set asks for features that were never
// offered, so a blank frame from here is not evidence of a defect in the
// port -- it is what should happen.
//
// Forcing the native host onto its GLES path (GEX_VIRGL_GLES) does not
// rescue it. Both native configurations advertise capability set version 2,
// 5,508 bytes of it; WebGL 2 can only offer version 1, 308 bytes. The guest
// asked for v2 and got it, so every recording made against this stack is
// unreplayable here, and no amount of matching the host API fixes that.
//
// So this is a smoke test with a hard boundary: it proves the transport, the
// framing and the command dispatch, and it cannot prove a picture. A picture
// needs the guest negotiating live against the renderer that will serve it.
// The pixel count below is reported, never asserted, for that reason.
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
// virglrenderer explains itself on stderr and says nothing through its return
// value beyond "no". Collect it, or a failure is just a number.
//
// Deduplicated, and that is not tidiness: a shader that will not compile is
// reported once and a buffer map that will not map is reported for every
// draw, so the tail of the log is guaranteed to be the harmless one.
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
      // Keep the biggest reply. A TRANSFER_GET of the frame is far larger
      // than anything else on the wire -- 300x300 RGBA is 360,000 bytes
      // against a few dozen for a fence or a capability answer -- so the
      // largest one is the picture, and the picture is the only thing that
      // says whether any of this actually drew.
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
      // The frame, as a PNG, so it can be looked at rather than counted.
      // Counted across the whole reply, not a slice of it. A pump can answer
      // two TRANSFER_GETs at once -- 729,648 bytes is two 300x300 frames and
      // some fences -- so any fixed offset into it is a guess, and a guess
      // that lands wrong reads zeros for the wrong reason.
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
