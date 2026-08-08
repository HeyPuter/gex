// A guest program asking a browser tab what its GPU can do.
//
//   node web/test/virgl-guest.mjs
//
// This is the end of the chain the rest of the virgl work has been building,
// and the first test of it that means anything.
//
// `web/test/virgl-webgl.mjs` replays a recorded stream, which proves the
// transport and can never prove more: the guest's Mesa asks what the host can
// do before it does anything, and every shader, format and draw follows from
// the answer. Recorded natively that answer is capability set version 2,
// 5,508 bytes of it; WebGL 2 can only answer version 1. A recording made
// against one host is not a valid input to the other, whichever API was used.
//
// So the guest has to ask for itself, and it does that here: translated
// aarch64 running in a page, opening a UNIX socket the browser shim serves,
// speaking vtest to virglrenderer compiled to wasm, and getting back an
// answer from the WebGL 2 context on the canvas. Nothing is recorded and
// nothing is replayed. What the program prints is what the renderer said.
//
// The number it prints is the point: a different capability set from the
// native one means the negotiation is live.
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
// A module that fails to load runs none of its own error handling, and a
// page that reports "pending" says nothing about which of the four things it
// loads was the problem. These do.
let step = 'start';
// Additive, because the marks and the guest's output share one element and
// a mark that replaced it threw away everything written so far -- which made
// the reported step wander depending on when the browser was dumped, and
// sent one investigation after a hang in the wrong function.
const mark = (s) => { step = s; el.textContent = 'at ' + s + '\\n' + output; };
addEventListener('error', (e) => say('ERROR at ' + step + ': ' + (e.message || e)));
addEventListener('unhandledrejection', (e) =>
  say('REJECTED at ' + step + ': ' + (e.reason?.message || e.reason)));

// Everything the guest writes, so the answer it got is the answer reported.
let output = '';
class PageEnv extends Env {
  // Straight into the DOM as well as the buffer. A hang reports whatever is
  // in the document when the browser is dumped, and the guest's own output is
  // the only thing that says how far it got -- setting textContent needs no
  // repaint, so it survives a page that never yields again.
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
  // Before load: the socket is bound while the guest's namespace is built,
  // and a guest that connects to a socket nobody serves does not fail, it
  // hangs -- Mesa's winsys writes into the dead descriptor and waits.
  // Tracing on: a stall then says where it stalled, through the guest's
  // own stderr, which in a page is the only channel there is.
  // GEX_VIRGL_OFF runs the identical page with the renderer not attached.
  // The guest then finds no socket and says so, which is a complete run --
  // so it separates a harness that cannot load a guest at all from one that
  // only fails when a renderer is present.
  if (!OFF) gex.useVirgl(renderer, TRACE);
  mark('loading the guest');
  const entry = await gex.load(
    await grab('./shim.wasm'), await grab('./guest.wasm'), await grab('./guest.elf'),
    {}, ['/virgl-handshake'], { uid: 0, gid: 0, cwd: '/' });
  mark('running the guest');
  // A guest that never finishes reports nothing at all, and "nothing at all"
  // does not say whether it got as far as connecting. The watchdog turns a
  // hang into whatever the program managed to print first.
  const status = await Promise.race([
    gex.run(entry),
    new Promise((r) => setTimeout(() => r('STUCK'), 20000)),
  ]);
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
      '--virtual-time-budget=60000',
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
