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

// The result goes into the title, which --dump-dom brings back. Anything
// printed to the console would need a debugging protocol to collect.
await writeFile(
  join(dir, 'index.html'),
  `<!doctype html><meta charset=utf-8><title>pending</title>
<canvas id="gex-gl" width="64" height="64"></canvas>
<pre id="out">pending</pre>
<script type="module">
import Module from './gexvirgl.mjs';
const say = (s) => { document.getElementById('out').textContent = s; document.title = s; };
// virglrenderer explains itself on stderr and says nothing through its return
// value beyond "no". Collect it, or a failure is just a number.
const log = [];
try {
  const m = await Module({ printErr: (s) => log.push(s), print: (s) => log.push(s) });
  const rc = m._gex_virgl_init(64, 64);
  if (rc !== 0) { say('INIT FAILED ' + rc + ' :: ' + log.join(' / ')); }
  else { say('OK ' + m.UTF8ToString(m._gex_virgl_describe()) + ' :: ' + log.join(' / ')); }
} catch (e) {
  say('THREW ' + (e && e.message ? e.message : String(e)) + ' :: ' + log.join(' / '));
}
</script>`,
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
  console.log('ok   virgl-webgl');
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
