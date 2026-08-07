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

// The result goes into the title, which --dump-dom brings back. Anything
// printed to the console would need a debugging protocol to collect.
await writeFile(
  join(dir, 'index.html'),
  `<!doctype html><meta charset=utf-8><title>pending</title>
<canvas id="gex-gl" width="300" height="300"></canvas>
<pre id="out">pending</pre>
<script type="module">
const REPLAY = __REPLAY__;
import Module from './gexvirgl.mjs';
const say = (s) => { document.getElementById('out').textContent = s; document.title = s; };
// virglrenderer explains itself on stderr and says nothing through its return
// value beyond "no". Collect it, or a failure is just a number.
const log = [];
try {
  const m = await Module({ printErr: (s) => log.push(s), print: (s) => log.push(s) });
  const rc = m._gex_virgl_init(300, 300);
  if (rc !== 0) { say('INIT FAILED ' + rc + ' :: ' + log.join(' / ')); }
  else if (!REPLAY) { say('OK ' + m.UTF8ToString(m._gex_virgl_describe()) + ' :: ' + log.join(' / ')); }
  else {
    const stream = new Uint8Array(await (await fetch('./stream.bin')).arrayBuffer());
    // In pieces, and deliberately not on command boundaries: the guest's
    // socket delivers whatever it has, so a command arriving in two halves is
    // the normal case and the rewind that handles it needs exercising.
    const CHUNK = 4093;
    let ran = 0, replies = 0, fed = 0, refused = false;
    const buf = m._malloc(CHUNK);
    for (let off = 0; off < stream.length && !refused; off += CHUNK) {
      const part = stream.subarray(off, Math.min(off + CHUNK, stream.length));
      m.HEAPU8.set(part, buf);
      m._gex_virgl_feed(buf, part.length);
      fed += part.length;
      const n = m._gex_virgl_pump();
      if (n < 0) { refused = true; break; }
      ran += n;
      replies += m._gex_virgl_pending();
      m._gex_virgl_drain();
    }
    m._free(buf);
    const tail = log.slice(-8).join(' / ');
    if (refused) {
      say('PUMP REFUSED after ' + ran + ' commands, ' + fed + ' of '
          + stream.length + ' bytes :: ' + tail);
    } else {
      say('OK replayed ' + stream.length + ' bytes, ' + ran + ' commands, '
          + replies + ' reply bytes :: ' + tail);
    }
  }
} catch (e) {
  say('THREW ' + (e && e.message ? e.message : String(e)) + ' :: ' + log.join(' / '));
}
</script>`.replace('__REPLAY__', String(replay)),
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
await rm(dir, { recursive: true, force: true });

console.log(`     ${said}`);
if (said.startsWith('OK ')) {
  console.log(replay ? 'ok   virgl-webgl (replayed a recorded stream)' : 'ok   virgl-webgl');
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
