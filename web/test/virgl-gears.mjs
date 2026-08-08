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
const __MISSES__ = __TRACEMISS__;
const __RECOMPILE__ = __RECOMPILEKIB__;
import { Gex, Env } from './gex.mjs';
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
let report = () => {};
let hotDump = () => {};
let drew = () => {};
class PageEnv extends Env {
  write(fd, bytes) {
    const text = new TextDecoder().decode(bytes);
    // When tracing, the guest's stderr goes out synchronously too, so that
    // it interleaves correctly with the renderer's own lines. Two channels
    // with different latencies produce a plausible ordering that is not the
    // real one, and the whole point of this trace is what came last.
    if (TRACE && fd === 2) { syncReport(text.trimEnd()); hotDump(); }
    output += text;
    paint();
    for (const line of text.split('\\n')) {
      if (line.trim()) report(line.trim());
      // glxgears never stops on its own, and the question this answers is
      // whether it draws at all -- so the first frame rate it prints is the
      // result, not something to wait past.
      if (/frames in .* seconds/.test(line)) drew(line.trim());
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

  const gex = new Gex(new PageEnv());
  gex.useVirgl(renderer, TRACE);
  if (TRACE) renderer.traceCommands(true);
  mark('loading the guest');
  const entry = await gex.load(
    await grab('./shim.wasm'), await grab('./seed.wasm'), files['/bin/sh'],
    files, ['/bin/sh', '/run.sh'], { uid: 0, gid: 0, cwd: '/' });
  mark('running the guest');
  // A sample every few seconds, into the same element. A run that does not
  // finish inside the budget is dumped where it stands, and without this the
  // page says only "at running the guest" -- which is true of a guest that
  // has drawn a thousand frames and of one that never opened the socket.
  const post = (kind, body) =>
    fetch('/report?kind=' + kind, { method: 'POST', body }).catch(() => {});
  const t0 = Date.now();
  // Fire and forget: awaiting here would hand the event loop back in the
  // middle of a guest write, which is not a safe place to give up the stack.
  // Blocks translated is the honest progress meter. The renderer's pump
  // count is not: the scheduler ticks whether or not the guest advances, so
  // a guest wedged on a socket and a guest translating libGL look identical
  // through it.
  //
  // Read in a microtask, not here. This runs inside the guest's write,
  // which the shim calls while it still holds its own state borrowed --
  // asking it anything from in here is a re-entrant borrow, and the run died
  // with 'RefCell already borrowed' before the X server had finished
  // starting. A microtask runs once the stack has unwound back to the page,
  // by which time the borrow is gone.
  const e = gex.shim.exports;
  // A line roughly once a second while the guest is executing, which is the
  // only window into a stretch that makes no syscalls and runs no JavaScript.
  e.gex_set_heartbeat?.(BigInt(process_HEARTBEAT));
  if (__MISSES__) e.gex_set_trace_misses?.(1);
  // The guest's hottest back edges, every so often, on the synchronous
  // channel. This is the browser's answer to the native host's --sample:
  // the heartbeat alone reports whichever address happened to be current,
  // which for a regular loop is the same two or three forever and says
  // nothing about the shape of the work.
  if (TRACE && e.gex_hot_blocks) {
    let n = 0;
    // One buffer, allocated once. And wrapped, because this is called from
    // inside the guest's own write: anything thrown here unwinds the guest
    // rather than the reporter, which ended three runs early and made the
    // samples look frozen when they were simply the last before the crash.
    let at = 0;
    hotDump = () => {
      if (++n % 4) return;
      try {
        if (!at) at = e.gex_alloc(2048) >>> 0;
        const slen = e.gex_recent_syscalls ? e.gex_recent_syscalls(at, 2048) >>> 0 : 0;
        if (slen) {
          syncReport('[last syscalls] ' +
            new TextDecoder().decode(new Uint8Array(gex.memory.buffer, at, slen))
              .trim().split('\\n').join(' | '));
        }
        const len2 = e.gex_hot_blocks(at, 2048) >>> 0;
        if (len2) {
          syncReport('[hot] ' +
            new TextDecoder().decode(new Uint8Array(gex.memory.buffer, at, len2))
              .trim().split('\\n').join(' | '));
        }
      } catch (err) {
        syncReport('[hot] sampling failed: ' + (err?.message || err));
        hotDump = () => {};
      }
    };
  }
  // Static recompilation: translate each shared object whole as the guest
  // maps it, rather than meeting it a dispatch miss at a time. The value is
  // a cap in KiB of mapped text -- see run-node.mjs for why 8192.
  if (__RECOMPILE__) e.gex_set_recompile_objects?.(__RECOMPILE__);
  // A pulse from the page itself, on the clock, owing nothing to the guest.
  //
  // Every other report in this harness is produced from inside the run: the
  // heartbeat needs a safepoint, the histogram needs a syscall, the census
  // needs the scheduler. A guest that stops reaching any of them goes silent,
  // and a silent run at 100% of a core has two completely different causes
  // that no existing instrument separates -- the guest is inside one long
  // stretch of translated code and JavaScript is not running at all, or
  // JavaScript is running perfectly well and something out here is spinning.
  //
  // This tells them apart by construction. The entry point does not return
  // until the guest traps, so while the guest holds the wasm stack no timer
  // can fire:
  // these lines stopping *is* the proof that the run is inside wasm, and
  // these lines continuing while nothing else reports puts the loop out here.
  setInterval(() => {
    const at = ((Date.now() - t0) / 1000).toFixed(0);
    post('progress', at + 's | [page] the event loop is still turning | ' +
      Number(e.gex_jit_blocks()) + ' blocks | ' + renderer.commands());
  }, 5000);
  const queued = [];
  report = (line) => {
    queued.push([((Date.now() - t0) / 1000).toFixed(0), line]);
    queueMicrotask(() => {
      const blocks = Number(e.gex_jit_blocks());
      const ms = Number(e.gex_jit_millis());
      while (queued.length) {
        const [at, l] = queued.shift();
        post('progress', at + 's | ' + l + ' | ' + blocks + ' blocks in ' + ms +
          'ms | ' + renderer.glState() + '\\n        ' + renderer.commands() +
          (rlog.length ? '\\n        renderer: ' + rlog.join(' / ') : ''));
      }
    });
  };
  // Whichever comes first: the guest finishing, or the guest drawing.
  const firstFrame = new Promise((r) => {
    drew = (line) => r('DREW ' + line);
  });
  const status = await Promise.race([gex.run(entry), firstFrame]);
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
    res.writeHead(200, { 'content-type': type });
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
