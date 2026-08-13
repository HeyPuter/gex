// A guest program asking a browser tab what its GPU can do.
//
//   node web/test/virgl-guest.mjs
//
// `virgl-webgl.mjs` replays a recording, which can only prove transport: Mesa's
// answer depends on the host's capability set (native is v2, 5,508 bytes; WebGL
// 2 answers v1), so a recording is not a valid input to the other host. Here
// the guest asks for itself -- translated aarch64 in a page, a UNIX socket the
// shim serves, vtest to virglrenderer (wasm), an answer from WebGL 2. Nothing
// is recorded. A capability set different from the native one means the
// negotiation is live.
import { spawn } from 'node:child_process';
import { mkdtemp, writeFile, rm, copyFile, access, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createServer } from 'node:http';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..', '..');
const BUILD = process.env.GEX_VIRGL_WASM || join(tmpdir(), 'gex-virgl-wasm');
const CHROME = process.env.CHROME || '/usr/bin/chromium';
const GUEST = process.env.GEX_VIRGL_GUEST || '';

const exists = async (p) => access(p).then(() => true, () => false);
const skip = (why) => {
  console.log(`skip: ${why}`);
  process.exit(0);
};

if (!(await exists(join(BUILD, 'gexvirgl.mjs')))) skip(`no renderer at ${BUILD}`);
if (!(await exists(CHROME))) skip(`no browser at ${CHROME}`);
if (!GUEST || !(await exists(GUEST))) skip('set GEX_VIRGL_GUEST to a built guest module');
const shim = join(ROOT, 'target/wasm32-unknown-unknown/release/gex_browser.wasm');
if (!(await exists(shim))) skip('no browser shim (run shims/browser/build.sh)');

const dir = await mkdtemp(join(tmpdir(), 'gex-virgl-guest-'));
for (const [from, to] of [
  [join(BUILD, 'gexvirgl.mjs'), 'gexvirgl.mjs'],
  [join(BUILD, 'gexvirgl.wasm'), 'gexvirgl.wasm'],
  [shim, 'shim.wasm'],
  [GUEST, 'guest.wasm'],
  [`${GUEST}.elf`, 'guest.elf'],
  [join(ROOT, 'shims/browser/js/gex.mjs'), 'gex.mjs'],
  [join(ROOT, 'web/src/virgl.js'), 'virgl.js'],
]) {
  if (await exists(from)) await copyFile(from, join(dir, to));
}
if (!(await exists(join(dir, 'guest.elf')))) skip(`no ${GUEST}.elf beside the module`);

await writeFile(
  join(dir, 'index.html'),
  `<!doctype html><meta charset=utf-8><title>pending</title>
<canvas id="gex-gl" width="300" height="300"></canvas>
<pre id="out">pending</pre>
<script type="module">
const OFF = __OFF__;
const TRACE = __TRACE__;
import { Gex, Env } from './gex.mjs';
import { VirglRenderer } from './virgl.js';
const el = document.getElementById('out');
const say = (s) => { el.textContent = s; document.title = s.split('\\n')[0]; };
// A module that fails to load runs no error handling of its own; these marks
// say which of the four loads was the problem.
let step = 'start';
// Additive: the marks and the guest's output share one element, and a
// replacing mark would throw away everything written so far.
const mark = (s) => { step = s; el.textContent = 'at ' + s + '\\n' + output; };
addEventListener('error', (e) => say('ERROR at ' + step + ': ' + (e.message || e)));
addEventListener('unhandledrejection', (e) =>
  say('REJECTED at ' + step + ': ' + (e.reason?.message || e.reason)));

// Everything the guest writes, so the answer it got is the answer reported.
let output = '';
class PageEnv extends Env {
  // Into the DOM as well as the buffer: a hang reports whatever the document
  // holds when dumped, and textContent needs no repaint, so it survives a page
  // that never yields again.
  write(fd, bytes) {
    output += new TextDecoder().decode(bytes);
    el.textContent = 'at ' + step + '\\n' + output;
  }
  now() { return BigInt(Date.now()) * 1000000n; }
}

try {
  mark('creating the renderer');
  const renderer = await VirglRenderer.create(document.getElementById('gex-gl'),
                                              './gexvirgl.mjs');
  if (!renderer.ok) { say('RENDERER FAILED: ' + renderer.error); throw 0; }

  mark('fetching the modules');
  const grab = async (u) => new Uint8Array(await (await fetch(u)).arrayBuffer());
  const gex = new Gex(new PageEnv());
  // Bound before load: a guest connecting to a socket nobody serves hangs
  // rather than failing (Mesa's winsys waits on the dead descriptor). Tracing
  // routes stalls through the guest's stderr, the only channel in a page.
  // GEX_VIRGL_OFF runs the same page with no renderer: the guest finds no
  // socket and says so, separating a load failure from a renderer failure.
  if (!OFF) gex.useVirgl(renderer, TRACE);
  mark('loading the guest');
  const entry = await gex.load(
    await grab('./shim.wasm'), await grab('./guest.wasm'), await grab('./guest.elf'),
    {}, ['/virgl-handshake'], { uid: 0, gid: 0, cwd: '/' });
  mark('running the guest');
  // No watchdog: the browser's virtual-time budget already bounds this, and
  // whatever the guest printed is in the document either way.
  const status = await gex.run(entry);
  say('EXIT ' + status + '\\n' + output.trim() + '\\nrenderer: ' + renderer.describe());
} catch (e) {
  if (e !== 0) say('THREW ' + (e?.message || String(e)) + '\\n' + output.trim());
}
</script>`.replace('__OFF__', String(!!process.env.GEX_VIRGL_OFF))
   .replace('__TRACE__', String(!!process.env.GEX_VIRGL_TRACE)),
);

const server = createServer(async (req, res) => {
  const name = (req.url === '/' ? '/index.html' : req.url).split('?')[0];
  try {
    const body = await readFile(join(dir, name));
    const type = name.endsWith('.wasm')
      ? 'application/wasm'
      : /\.(mjs|js)$/.test(name)
        ? 'text/javascript'
        : name.endsWith('.elf')
          ? 'application/octet-stream'
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
  const p = spawn(
    CHROME,
    [
      '--headless',
      '--no-sandbox',
      '--disable-gpu-sandbox',
      '--enable-unsafe-swiftshader',
      '--use-gl=swiftshader',
      // Ten minutes of virtual time: the clock only advances while the page is
      // idle, and this page is busy translating and running an aarch64 program,
      // so a short budget dumps mid-run and reads as a hang.
      '--virtual-time-budget=' + (process.env.GEX_BUDGET || 600000),
      '--dump-dom',
      url,
    ],
    { stdio: ['ignore', 'pipe', 'pipe'] },
  );
  let out = '';
  let err = '';
  p.stdout.on('data', (d) => (out += d));
  p.stderr.on('data', (d) => (err += d));
  p.on('error', reject);
  p.on('close', () => resolve({ out, err }));
});
server.close();

const m = /<pre id="out">([\s\S]*?)<\/pre>/.exec(dom.out);
const said = (m ? m[1] : '(no result element)').replace(/&quot;/g, '"');
await rm(dir, { recursive: true, force: true });

console.log(said.split('\n').map((l) => `     ${l}`).join('\n'));
if (/^EXIT 0$/m.test(said) && /capset read whole/.test(said)) {
  console.log('ok   virgl-guest (a guest negotiated live with WebGL 2)');
  process.exit(0);
}
console.log('FAIL virgl-guest: the guest did not get a capability set from the page');
console.log(
  dom.err.split('\n').filter((l) => l && !l.includes('Fontconfig')).slice(-10)
    .map((l) => `     ${l}`).join('\n'),
);
process.exit(1);
