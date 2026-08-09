// Debian's glxgears, drawing on WebGL 2, in a headless browser.
//
//   node web/test/virgl-gears.mjs
//
// `virgl-guest.mjs` proved the negotiation: a translated aarch64 program
// asked a page what its GPU could do and got an answer from WebGL 2 rather
// than from a recording. That is the handshake and only the handshake --
// nothing had ever been drawn through it.
//
// This is the drawing. The guest is Debian's own aarch64 `glxgears` against
// Debian's own Mesa, with an `Xvfb` beside it, all of it translated in the
// page. Mesa's `virgl` Gallium driver writes the command stream it would
// write to a real GPU, and virglrenderer -- compiled to wasm -- executes it
// on the canvas.
//
// Two things have to be true for this to pass, and they fail differently:
//
//   * glxgears prints a frame rate. That means Mesa completed frames and got
//     its pixels back, which is the whole guest half of the chain.
//   * the renderer reports having presented. TRANSFER_GET is a readback, not
//     a present, so a guest can render perfectly and leave the canvas black
//     forever; `presented` counts the blits that put a frame on it.
//
// It is slow. A cold run translates Mesa, an X server and glibc through the
// runtime translator, and that is minutes rather than seconds -- which is
// exactly the argument for the static recompiler, and is why the budget
// below is what it is.
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
const TAR = process.env.GEX_GRAPHICS_TAR || join(ROOT, 'web/public/rootfs/graphics.tar');
const SECONDS = process.env.GEX_GEARS_SECONDS || '0';

const exists = async (p) => access(p).then(() => true, () => false);
const skip = (why) => {
  console.log(`skip: ${why}`);
  process.exit(0);
};

if (!(await exists(join(BUILD, 'gexvirgl.mjs')))) skip(`no renderer at ${BUILD}`);
if (!(await exists(CHROME))) skip(`no browser at ${CHROME}`);
if (!(await exists(TAR))) skip(`no graphics rootfs at ${TAR} (web/build-graphics-rootfs.sh)`);
const shim = join(ROOT, 'target/wasm32-unknown-unknown/release/gex_browser.wasm');
const seed = join(ROOT, 'web/public/gex/seed.wasm');
if (!(await exists(shim))) skip('no browser shim (run shims/browser/build.sh)');
if (!(await exists(seed))) skip('no seed module (run web/build-assets.sh)');

const dir = await mkdtemp(join(tmpdir(), 'gex-virgl-gears-'));
for (const [from, to] of [
  [join(BUILD, 'gexvirgl.mjs'), 'gexvirgl.mjs'],
  [join(BUILD, 'gexvirgl.wasm'), 'gexvirgl.wasm'],
  [shim, 'shim.wasm'],
  [seed, 'seed.wasm'],
  [TAR, 'graphics.tar'],
  [join(ROOT, 'shims/browser/js/gex.mjs'), 'gex.mjs'],
  [join(ROOT, 'shims/browser/js/host.mjs'), 'host.mjs'],
  [join(ROOT, 'shims/browser/js/worker.mjs'), 'worker.mjs'],
  [join(ROOT, 'shims/browser/js/virgl-bridge.mjs'), 'virgl-bridge.mjs'],
  [join(ROOT, 'web/public/gex/gex_kernel.plain.wasm'), 'gex_kernel.plain.wasm'],
  [join(ROOT, 'web/src/virgl.js'), 'virgl.js'],
  [join(ROOT, 'web/src/tar.js'), 'tar.js'],
]) {
  await copyFile(from, join(dir, to));
}

await writeFile(
  join(dir, 'index.html'),
  `<!doctype html><meta charset=utf-8><title>pending</title>
<canvas id="gex-gl" width="300" height="300"></canvas>
<pre id="out">pending</pre>
<script type="module">
const SECONDS = '__SECONDS__';
const TRACE = __TRACE__;
const process_HEARTBEAT = __HEARTBEAT__;
const WANT_REPORTS = __REPORTS__;
const __MISSES__ = __TRACEMISS__;
const __RECOMPILE__ = __RECOMPILEKIB__;
import { Env } from './gex.mjs';
import { ProcessHost } from './host.mjs';
import { VirglRenderer } from './virgl.js';
import { untar } from './tar.js';
const el = document.getElementById('out');
let output = '';
let step = 'start';
const paint = () => { el.textContent = 'at ' + step + '\\n' + output; };
const mark = (s) => { step = s; paint(); };
const say = (s) => { el.textContent = s; document.title = s.split('\\n')[0]; };
addEventListener('error', (e) => say('ERROR at ' + step + ': ' + (e.message || e)));
addEventListener('unhandledrejection', (e) =>
  say('REJECTED at ' + step + ': ' + (e.reason?.message || e.reason)));

// Progress is reported from the guest's own writes, not from a timer.
//
// A timer cannot work here: the run loop yields to the event loop only when
// it pauses, and while it is translating Mesa it does not pause for minutes
// at a time -- so setInterval fired twice, at ten and twenty seconds, and
// then said nothing for the rest of the run. Silence from a harness is worse
// than no harness: it cannot tell slow from wedged.
//
// The guest writes a line each time it reaches a stage, and those writes come
// through here.
function syncReport(line) {
  try {
    const x = new XMLHttpRequest();
    x.open('POST', '/report?kind=progress', false);
    x.send(line);
  } catch (e) { /* the server has gone; nothing to be done from here */ }
}
const rates = [];
let report = () => {};
let drew = () => {};
class PageEnv extends Env {
  write(fd, bytes) {
    const text = new TextDecoder().decode(bytes);
    // When tracing, the guest's stderr goes out synchronously too, so that
    // it interleaves correctly with the renderer's own lines. Two channels
    // with different latencies produce a plausible ordering that is not the
    // real one, and the whole point of this trace is what came last.
    if (TRACE && fd === 2) syncReport(text.trimEnd());
    output += text;
    paint();
    for (const line of text.split('\\n')) {
      if (line.trim()) report(line.trim());
      // glxgears never stops on its own, and the question this answers is
      // whether it draws at all -- so the first frame rate it prints is the
      // result, not something to wait past.
      // glxgears prints a rate every five seconds. The first one is a cold
      // run -- it counts the frames drawn while Mesa, an X server and glibc
      // were still being translated a basic block at a time -- so waiting
      // for a later one is the only way to see sustained speed.
      if (/frames in .* seconds/.test(line)) {
        rates.push(line.trim());
        if (rates.length >= WANT_REPORTS) drew(rates.join(' ; '));
      }
    }
  }
  now() { return BigInt(Date.now()) * 1000000n; }
}

try {
  mark('creating the renderer');
  // The renderer's diagnostics through this harness's own channel, not the
  // console. It is a separate wasm module with its own libc and cannot reach
  // the shim's stderr, and the headless browser's console is unreliable
  // under load -- four lines of twenty arrived, which reads like the whole
  // trace and is not.
  // Delivered synchronously when tracing, with a blocking XMLHttpRequest.
  //
  // Everything else in this harness reports through fetch, which needs the
  // event loop to turn. That is fine until the thing being diagnosed is a
  // call that does not return: the line naming it is written, queued, and
  // never sent, so the last thing reported is always the last thing before
  // the interesting one. A synchronous request is slow and rude and arrives.
  const rsend = (s) => syncReport('[virgl] ' + s);

  const rlog = [];
  const renderer = await VirglRenderer.create(document.getElementById('gex-gl'),
                                              './gexvirgl.mjs',
                                              (s) => {
                                                if (TRACE) { rsend(s); return; }
                                                rlog.push(s);
                                                if (rlog.length > 6) rlog.shift();
                                              });
  if (!renderer.ok) { say('RENDERER FAILED: ' + renderer.error); throw 0; }

  mark('unpacking the rootfs');
  const grab = async (u) => new Uint8Array(await (await fetch(u)).arrayBuffer());
  const tarBytes = await grab('./graphics.tar');
  const tree = untar(tarBytes.buffer.slice(tarBytes.byteOffset,
                                           tarBytes.byteOffset + tarBytes.byteLength));
  const files = { ...tree.files };
  for (const [path, target] of Object.entries(tree.links)) {
    const abs = target.startsWith('/')
      ? target
      : (path.slice(0, path.lastIndexOf('/')) + '/' + target).replace(/\\/{2,}/g, '/');
    if (files[abs]) files[path] = files[abs];
  }
  // The browser loader has no way to pass an environment, so the bound is
  // written into a script instead of exported around one.
  files['/run.sh'] = new TextEncoder().encode(
    'GEARS_SECONDS=' + SECONDS + '\\nexport GEARS_SECONDS\\n. /gears.sh\\n');
  output += Object.keys(files).length + ' files in the image\\n';

  // The guest runs in Workers -- one per process, which is what replaced the
  // snapshot switcher. That matters here more than anywhere: this demo is a
  // shell that starts an X server in the background and then runs glxgears,
  // so the process that draws is two forks and an execve away from pid 1.
  //
  // The renderer stays on this thread, because it is the thread with the
  // canvas. What crosses is a SharedArrayBuffer: the drawing process writes
  // its command stream into it, wakes this page, and blocks on
  // Atomics.wait until the reply is there.
  const { makeChannel, serveVirgl } = await import('./virgl-bridge.mjs');
  const channel = makeChannel();
  const serve = serveVirgl(renderer, channel);
  if (TRACE) renderer.traceCommands(true);

  mark('loading the guest');
  const t0 = Date.now();
  const post = (kind, body) =>
    fetch('/report?kind=' + kind, { method: 'POST', body }).catch(() => {});
  const env = new PageEnv();

  const kernelBytes = await grab('./gex_kernel.plain.wasm');
  const host = new ProcessHost({
    makeWorker: () => new Worker('./worker.mjs', { type: 'module' }),
    shim: await grab('./shim.wasm'),
    guest: await grab('./seed.wasm'),
    kernel: {
      module: await WebAssembly.compile(kernelBytes),
      memory: new WebAssembly.Memory({ initial: 2048, maximum: 16384, shared: true }),
    },
    extra: {
      virgl: channel,
      virglTrace: TRACE,
      recompileObjects: __RECOMPILE__ || undefined,
      heartbeat: process_HEARTBEAT || undefined,
    },
    onOutput: (fd, text) => env.write(fd, new TextEncoder().encode(text)),
    onMessage: (m) => {
      // The drawing process is asking for the renderer. It is blocked on
      // Atomics.wait until this returns, so nothing may be awaited here.
      if (m.kind === 'virgl') serve();
    },
  });

  mark('running the guest');
  // A pulse from the page itself, on the clock, owing nothing to the guest.
  //
  // It means something different now that the guest is in a Worker: this
  // thread is free, so these lines continuing proves only that the page is
  // alive. What they carry -- the renderer's command histogram -- is the
  // real progress meter, because every one of those commands came from the
  // guest and crossed through here.
  setInterval(() => {
    const at = ((Date.now() - t0) / 1000).toFixed(0);
    post('progress', at + 's | [page] ' + host.pids.length + ' processes | ' +
      renderer.commands());
  }, 5000);
  report = (line) => {
    post('progress', ((Date.now() - t0) / 1000).toFixed(0) + 's | ' + line +
      ' | ' + renderer.glState() + '\\n        ' + renderer.commands() +
      (rlog.length ? '\\n        renderer: ' + rlog.join(' / ') : ''));
  };
  // Whichever comes first: the guest finishing, or the guest drawing.
  const firstFrame = new Promise((r) => {
    drew = (line) => r('DREW ' + line);
  });
  const status = await Promise.race([
    host.start({ elf: files['/bin/sh'], files, args: ['/bin/sh', '/run.sh'],
                 opts: { uid: 0, gid: 0, cwd: '/' } }),
    firstFrame,
  ]);
  const result = 'EXIT ' + status + '\\n' + output.trim() +
      '\\nrenderer: ' + renderer.describe() +
      '\\nstate: ' + renderer.glState() +
      '\\ncommands: ' + renderer.commands();
  say(result);
  await post('done', result);
} catch (e) {
  if (e !== 0) {
    const result = 'THREW ' + (e?.message || String(e)) + '\\n' + output.trim();
    say(result);
    await fetch('/report?kind=done', { method: 'POST', body: result }).catch(() => {});
  }
}
</script>`.replace('__SECONDS__', SECONDS)
   .replace('__TRACE__', String(!!process.env.GEX_VIRGL_TRACE))
   .replace('__HEARTBEAT__', String(Number(process.env.GEX_HEARTBEAT || 0)))
   .replace('__TRACEMISS__', process.env.GEX_TRACE_MISSES ? 'true' : 'false')
   .replace('__REPORTS__', String(Number(process.env.GEX_GEARS_REPORTS) || 1))
   .replace('__RECOMPILEKIB__', String(
     process.env.GEX_RECOMPILE_OBJECTS === undefined
       ? 0
       : Number(process.env.GEX_RECOMPILE_OBJECTS) || 8192,
   )),
);

let finish;
const finished = new Promise((r) => (finish = r));
const server = createServer(async (req, res) => {
  const name = (req.url === '/' ? '/index.html' : req.url).split('?')[0];
  // The page reports rather than being dumped. `--virtual-time-budget` was
  // the wrong tool: it advances the clock for every timer, so a five-second
  // sampler burned 1,750 seconds of a half-hour budget in two minutes of real
  // time and the browser was killed with the X server still starting. Real
  // time and a report make the run as long as it needs to be, and say where
  // it is while it lasts.
  if (name === '/report') {
    const chunks = [];
    for await (const c of req) chunks.push(c);
    const body = Buffer.concat(chunks).toString();
    const kind = new URL(req.url, 'http://x').searchParams.get('kind');
    if (kind === 'progress') {
      // Every line, indented under the first. This took the first only, and
      // silently ate the command histogram it was asked to print.
      if (!process.env.GEX_QUIET) {
        const [first, ...rest] = body.split('\n');
        console.log(`     ... ${first}`);
        for (const l of rest) console.log(`         ${l.trim()}`);
      }
    } else {
      finish(body);
    }
    res.writeHead(204).end();
    return;
  }
  try {
    const body = await readFile(join(dir, name));
    const type = name.endsWith('.wasm')
      ? 'application/wasm'
      : /\.(mjs|js)$/.test(name)
        ? 'text/javascript'
        : name.endsWith('.tar')
          ? 'application/x-tar'
          : 'text/html';
    // Cross-origin isolation, because the runtime wants a
    // `SharedArrayBuffer`: the global kernel's memory is one, and so is a
    // process whose threads are Workers. Without these headers the
    // constructor is not there at all.
    res.writeHead(200, {
      'content-type': type,
      'Cross-Origin-Opener-Policy': 'same-origin',
      'Cross-Origin-Embedder-Policy': 'require-corp',
    });
    res.end(body);
  } catch {
    res.writeHead(404).end();
  }
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const url = `http://127.0.0.1:${server.address().port}/`;

const child = spawn(
  CHROME,
  [
    '--headless',
    '--no-sandbox',
    '--disable-gpu-sandbox',
    '--enable-unsafe-swiftshader',
    // Without this the page's console goes nowhere. `console.warn` is where
    // the renderer's own diagnostics come out -- it is a separate wasm
    // module with its own libc, so it cannot reach the shim's stderr -- and
    // a trace that is never delivered is worse than no trace.
    '--enable-logging=stderr',
    '--v=0',
    // GEX_PROFILE=1 turns on V8's sampling profiler. It samples from a signal
    // handler, so it reports where a run is even while the guest holds the
    // wasm stack and nothing in this harness can say anything -- which is the
    // one state where every other instrument here is silent by construction.
    // The log lands in the served directory, next to the page.
    ...(process.env.GEX_PROFILE
      ? [`--js-flags=--prof --logfile=${process.env.GEX_PROFILE}`]
      : []),
    url,
  ],
  { stdio: ['ignore', 'pipe', 'pipe'] },
);
let err = '';
child.stderr.on('data', (d) => (err += d));

const WALL = Number(process.env.GEX_WALL || 3600) * 1000;
const said = await Promise.race([
  finished,
  new Promise((r) => setTimeout(() => r(null), WALL)),
]);
child.kill('SIGKILL');
server.close();
await rm(dir, { recursive: true, force: true });

if (said === null) {
  console.log(`FAIL virgl-gears: nothing reported in ${WALL / 1000}s`);
  console.log(err.split('\n').filter((l) => l && !l.includes('Fontconfig')).slice(-10)
    .map((l) => `     ${l}`).join('\n'));
  process.exit(1);
}
const dom = { err };

console.log(said.split('\n').map((l) => `     ${l}`).join('\n'));

const drew = /frames in .* seconds/.test(said);
const presented = Number(/presented=(\d+)/.exec(said)?.[1] || 0);
if (drew && presented > 0) {
  console.log(`ok   virgl-gears (glxgears drew, and ${presented} frames reached the canvas)`);
  process.exit(0);
}
console.log(
  `FAIL virgl-gears: ${drew ? 'glxgears drew' : 'glxgears never reported a frame rate'}` +
    `, ${presented} frames reached the canvas`,
);
console.log(
  dom.err.split('\n').filter((l) => l && !l.includes('Fontconfig')).slice(-15)
    .map((l) => `     ${l}`).join('\n'),
);
process.exit(1);
