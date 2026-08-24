// A fast, persistent, well-instrumented driver for the demo site.
//
//   node web/test/drive.mjs virgl                 # run the glxgears+Mesa demo
//   node web/test/drive.mjs virgl --build         # build.sh first, then run
//   node web/test/drive.mjs gears --query gears_size=100x100
//   node web/test/drive.mjs debian --cmd 'ls /; echo done' --want 'done'
//
// Why this exists alongside `site.mjs`: `site.mjs` cold-spawns a headless
// chromium per run and kills it, and only ever sees the browser's flattened
// stderr. This keeps ONE browser and ONE static server warm across every
// iteration (the only per-run cost left is your rebuild), and drives the page
// over CDP so it can see what the spawn model cannot:
//
//   * every Worker's console and every uncaught exception -- gex runs each
//     guest process in a dedicated Worker, so the trap that matters happens
//     THERE, invisible to a stderr scrape. FLAT auto-attach surfaces it here,
//     tagged with which worker it came from.
//   * a renderer/worker CRASH as a crash ("likely OOM"), not a mystery timeout.
//   * a HANG as a hang: if neither the guest terminal nor any console moves for
//     `--idle` seconds while the demo has not reached its goal, the run stops
//     early and labelled, instead of waiting out the wall.
//
// Everything is streamed as it happens (guest terminal lines, every console
// error, worker attach/detach/crash), because a silent run cannot be told from
// a stuck one. Exit code is 0 on pass, non-zero on fail/hang/crash.

import { spawn } from 'node:child_process';
import { mkdirSync, openSync, existsSync, writeFileSync } from 'node:fs';
import { readFile } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';
import { Cdp, browserWsUrl } from './cdp.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const WEB = join(HERE, '..');
const REPO = join(WEB, '..');

// Stable, NOT the session scratchpad: the browser and server are meant to
// outlive one invocation and be reused by the next.
const STATE = '/tmp/gex-harness';
mkdirSync(STATE, { recursive: true });

const CHROME = process.env.CHROME || '/usr/bin/chromium';
const CHROME_PORT = Number(process.env.GEX_CHROME_PORT || 9333);
const SERVE_PORT = Number(process.env.GEX_SERVE_PORT || 8090);

// ---------------------------------------------------------------------------
// What each demo is, and what counts as having worked. `want`/`fail` may be a
// RegExp or a (terminalText) => bool; kept byte-for-byte in step with
// site.mjs's table for the demos it also has. `settle` is a gex:status text
// that ends the wait early. `query` is appended to the page URL.
// ---------------------------------------------------------------------------
const DEMOS = {
  busybox: { want: /~ \$/ },
  debian: { want: /[#$]/ },
  // The API-split WebGL path (guest links libGLESv2/libEGL marshallers): its
  // own glReadPixels coverage, so a black frame that exits 0 does not pass.
  gears: { want: /covered ([1-9]\d*)%/, fail: /covered 0%/, settle: 'finished' },
  // glxgears on Debian's own Mesa via virglrenderer. Two things must hold: a
  // frame rate (Mesa completed frames) AND that a lit texture reached the
  // canvas -- `lit=0` is the draws-but-presents-black failure this is for.
  virgl: {
    want: (t) =>
      /frames in [\d.]+ seconds/.test(t) &&
      Number(/lit=(\d+)/.exec(t.replace(/\s+/g, ''))?.[1]) > 0,
    fail: (t) => /lit=0(?!\d)/.test(t.replace(/\s+/g, '')),
  },
  // glxgears under Xwayland, GL through virgl (same success bar as `virgl`):
  // a frame rate AND a lit texture presented. `lit=0` is drew-nothing.
  wayland: {
    want: (t) =>
      /frames in [\d.]+ seconds/.test(t) &&
      Number(/lit=(\d+)/.exec(t.replace(/\s+/g, ''))?.[1]) > 0,
    fail: (t) => /lit=0(?!\d)/.test(t.replace(/\s+/g, '')),
    settle: 'finished',
  },
};

// Dispatched in the page by `--input`: synthetic mouse and keyboard on the
// compositor canvas (`#gex-wl`), the same DOM events a real user would make, so
// `attachWlInput`'s listeners fire and fill the input ring. Coordinates are
// element-relative; the keys are `KeyboardEvent.code`s the evdev table maps.
const INJECT_JS = `(() => {
  const cv = document.getElementById('gex-wl');
  if (!cv) return 'no #gex-wl canvas';
  cv.focus?.();
  const r = cv.getBoundingClientRect();
  const cx = r.left + r.width / 2, cy = r.top + r.height / 2;
  const move = (x, y) => cv.dispatchEvent(new MouseEvent('mousemove', { clientX: x, clientY: y, bubbles: true }));
  // A grid sweep over the WHOLE element, so the surface-mapped pointer visits
  // every region -- a window in a corner (xeyes) is reached as surely as one in
  // the middle (gears).
  for (let gy = 0; gy <= 6; gy++)
    for (let gx = 0; gx <= 6; gx++)
      move(r.left + (r.width * gx) / 6, r.top + (r.height * gy) / 6);
  cv.dispatchEvent(new MouseEvent('mousedown', { button: 0, clientX: cx, clientY: cy, bubbles: true }));
  cv.dispatchEvent(new MouseEvent('mouseup', { button: 0, clientX: cx, clientY: cy, bubbles: true }));
  for (const code of ['ArrowUp', 'ArrowLeft', 'KeyA', 'ArrowRight', 'ArrowDown']) {
    cv.dispatchEvent(new KeyboardEvent('keydown', { code, bubbles: true }));
    cv.dispatchEvent(new KeyboardEvent('keyup', { code, bubbles: true }));
  }
  return 'dispatched';
})()`;

// Counts non-black pixels on the compositor canvas (#gex-wl) -- the read-back
// that tells `--wllit` a client drew something the whole pipeline delivered.
const WLLIT_JS = `(() => {
  const cv = document.getElementById('gex-wl');
  if (!cv) return 0;
  const ctx = cv.getContext('2d');
  if (!ctx) return 0;
  const d = ctx.getImageData(0, 0, cv.width, cv.height).data;
  let lit = 0;
  for (let i = 0; i < d.length; i += 4) if (Math.max(d[i], d[i + 1], d[i + 2]) > 24) lit++;
  return lit;
})()`;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------
function parseArgs(argv) {
  const o = {
    demo: 'virgl',
    build: false,
    wallMs: 240_000,
    idleMs: 60_000,
    keep: false,
    allowErrors: false,
    query: [],
    cmd: null,
    want: null,
    fail: null,
    shot: null,
    restartChrome: false,
    // Don't enable `Runtime` on worker targets. The inspector retains the
    // arguments of every console message it is shown, and gex runs a hundred
    // Workers that each log; measuring memory with that on measures the
    // observer too. This is how to tell how much of a peak is ours.
    quietWorkers: false,
  };
  const rest = [];
  for (let i = 2; i < argv.length; i++) {
    const a = argv[i];
    const next = () => argv[++i];
    if (a === '--build') o.build = true;
    else if (a === '--keep') o.keep = true;
    else if (a === '--allow-errors') o.allowErrors = true;
    else if (a === '--restart-chrome') o.restartChrome = true;
    else if (a === '--quiet-workers') o.quietWorkers = true;
    else if (a === '--wall') o.wallMs = Number(next()) * 1000;
    else if (a === '--idle') o.idleMs = Number(next()) * 1000;
    else if (a === '--query') o.query.push(next());
    else if (a === '--cmd') o.cmd = next();
    else if (a === '--want') o.want = next();
    else if (a === '--fail') o.fail = next();
    else if (a === '--shot') o.shot = next();
    else if (a === '--input') o.input = true;
    else if (a === '--applit') o.applit = true;
    else if (a === '--wllit') o.wllit = true;
    else if (a.startsWith('--')) fatal(`unknown flag ${a}`);
    else rest.push(a);
  }
  if (rest[0]) o.demo = rest[0];
  return o;
}

function fatal(msg) {
  console.error(`drive: ${msg}`);
  process.exit(2);
}

const stamp = () => new Date().toISOString().slice(11, 23);
const log = (...a) => console.log(`${stamp()}`, ...a);

// ---------------------------------------------------------------------------
// The persistent static server: web/dist, cross-origin isolated. Reuse the one
// already listening if it is healthy, else launch serve-dist.mjs detached so it
// survives this process and the next run finds it up.
// ---------------------------------------------------------------------------
async function serverHealthy(port) {
  try {
    const r = await fetch(`http://127.0.0.1:${port}/index.html`, {
      signal: AbortSignal.timeout(1500),
    });
    // The isolation header is the thing that actually matters: without it there
    // is no SharedArrayBuffer and threads/the global kernel vanish silently.
    return r.ok && r.headers.get('cross-origin-opener-policy') === 'same-origin';
  } catch {
    return false;
  }
}

async function ensureServer() {
  if (await serverHealthy(SERVE_PORT)) {
    log(`server: reusing http://127.0.0.1:${SERVE_PORT}/`);
    return;
  }
  if (!existsSync(join(WEB, 'dist', 'index.html'))) {
    fatal(`no web/dist/index.html -- run \`bash shims/browser/build.sh\` (or pass --build)`);
  }
  const out = openSync(join(STATE, 'serve.log'), 'a');
  const child = spawn('node', [join(WEB, 'serve-dist.mjs'), String(SERVE_PORT)], {
    cwd: REPO,
    detached: true,
    stdio: ['ignore', out, out],
  });
  child.unref();
  for (let i = 0; i < 60; i++) {
    if (await serverHealthy(SERVE_PORT)) {
      log(`server: launched http://127.0.0.1:${SERVE_PORT}/ (pid ${child.pid})`);
      return;
    }
    await sleep(250);
  }
  fatal('server did not come up (see /tmp/gex-harness/serve.log)');
}

// ---------------------------------------------------------------------------
// The persistent browser: one headless chromium with remote debugging, a fixed
// profile so a second launch refuses (singleton) and a fixed port so we find
// it again. WebGL via swiftshader (the demos need a real GL context headless).
// ---------------------------------------------------------------------------
async function ensureChrome(restart) {
  if (restart) {
    const url = await browserWsUrl(CHROME_PORT);
    if (url) {
      try {
        const c = await Cdp.connect(url);
        await c.send('Browser.close');
      } catch {
        /* fall through to killing by pidfile */
      }
    }
    try {
      const pid = Number(await readFile(join(STATE, 'chrome.pid'), 'utf8'));
      if (pid) process.kill(pid, 'SIGKILL');
    } catch {
      /* none recorded */
    }
    await sleep(500);
  }

  let url = await browserWsUrl(CHROME_PORT);
  if (url) {
    log(`chrome: reusing :${CHROME_PORT}`);
    return url;
  }
  if (!existsSync(CHROME)) fatal(`no browser at ${CHROME} (set CHROME=...)`);

  const out = openSync(join(STATE, 'chrome.log'), 'a');
  const args = [
    '--headless',
    '--no-sandbox',
    '--disable-gpu-sandbox',
    // A real, if software, WebGL2 context in headless -- the graphics demos
    // negotiate a GL context before drawing a byte.
    '--enable-unsafe-swiftshader',
    '--use-angle=swiftshader-webgl',
    '--enable-gpu-rasterization',
    '--ozone-platform=headless',
    '--ozone-override-screen-size=800,600',
    // We do our own hang detection and want the real stack, not Chrome's
    // "page unresponsive" interposing.
    '--disable-hang-monitor',
    // `performance.memory` for the heap peak, our lightest OOM signal.
    '--enable-precise-memory-info',
    '--enable-logging=stderr',
    '--v=0',
    '--no-first-run',
    '--no-default-browser-check',
    '--remote-allow-origins=*',
    `--remote-debugging-port=${CHROME_PORT}`,
    `--user-data-dir=${join(STATE, 'chrome-profile')}`,
    'about:blank',
  ];
  const child = spawn(CHROME, args, { detached: true, stdio: ['ignore', out, out] });
  child.unref();
  writeFileSync(join(STATE, 'chrome.pid'), String(child.pid));
  for (let i = 0; i < 80; i++) {
    url = await browserWsUrl(CHROME_PORT);
    if (url) {
      log(`chrome: launched :${CHROME_PORT} (pid ${child.pid})`);
      return url;
    }
    await sleep(250);
  }
  fatal('chrome did not expose a debugging port (see /tmp/gex-harness/chrome.log)');
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Total RSS of OUR chromium instance, in MiB -- our early OOM warning: the big
// gex memory is a SharedArrayBuffer, which never shows in JS-heap metrics but
// does show here, so a climb toward the ceiling is visible before the kill.
// Matched by our unique `--user-data-dir`, so an unrelated chromium on the box
// (a stray headless run) does not inflate the number and mislead the guard.
const PROFILE = join(STATE, 'chrome-profile');
function chromeRssMiB() {
  try {
    // `ps` with the full command line, summing the RSS of every process whose
    // args name our profile -- the browser process and all its renderers/gpu.
    const out = execFileSync('ps', ['-eo', 'rss=,args='], { encoding: 'utf8' });
    let kb = 0;
    for (const line of out.split('\n')) {
      if (line.includes(PROFILE)) kb += Number(line.trim().split(/\s+/)[0]) || 0;
    }
    return Math.round(kb / 1024);
  } catch {
    return 0;
  }
}

// ---------------------------------------------------------------------------
// One run.
// ---------------------------------------------------------------------------
async function runDemo(o) {
  const spec = DEMOS[o.demo] ?? {};
  const want = compileMatcher(o.want) ?? spec.want ?? (() => false);
  const fail = compileMatcher(o.fail) ?? spec.fail ?? (() => false);
  const settle = spec.settle ?? null;

  const qs = new URLSearchParams();
  qs.set('demo', spec.demo ?? o.demo);
  qs.set('run', '1');
  if (o.cmd ?? spec.cmd) qs.set('cmd', o.cmd ?? spec.cmd);
  if (o.allowErrors) qs.set('allowerrors', '1');
  for (const q of [spec.query, ...o.query].filter(Boolean)) {
    for (const [k, v] of new URLSearchParams(q)) qs.set(k, v);
  }
  const url = `http://127.0.0.1:${SERVE_PORT}/?${qs}`;
  log(`run: ${url}`);
  log(`goal: ${o.want ? `--want ${o.want}` : describe(spec.want)}${settle ? `  (settle: ${settle})` : ''}`);

  const browser = await Cdp.connect(await ensureChrome(o.restartChrome));

  // A fresh tab per run: a wedged worker from a previous run must not poison
  // the next, and closing the tab (not the browser) keeps startup instant.
  const { targetId } = await browser.send('Target.createTarget', { url: 'about:blank' });
  const { sessionId: page } = await browser.send('Target.attachToTarget', {
    targetId,
    flatten: true,
  });

  // --- diagnostics: nothing interesting must be able to happen unseen -------
  const errors = []; // console.error / exception / crash, streamed AND kept
  const workers = new Map(); // sessionId -> label
  let crashed = null;
  let lastActivity = Date.now();
  const note = () => (lastActivity = Date.now());

  const label = (sid) => (sid && sid !== page ? workers.get(sid) ?? `worker?` : 'page');

  browser.on('Runtime.consoleAPICalled', (p, sid) => {
    note();
    const text = (p.args ?? []).map(previewArg).join(' ');
    if (p.type === 'error' || p.type === 'assert') {
      record(errors, `[${label(sid)}] console.${p.type}: ${text}`);
    } else if (p.type === 'warning') {
      log(`  warn [${label(sid)}] ${trunc(text, 200)}`);
    } else if (/\bquiet:|MISS-LOOP|\[virgl\] running/.test(text)) {
      // The wait census (console.debug "[gex] quiet: ...") and a few other
      // diagnostics say what a wedged run is parked on -- surfaced on demand.
      log(`  · [${label(sid)}] ${trunc(text, 300)}`);
    }
    // Other info/log/debug is the guest's own chatter; the terminal carries the
    // meaningful part, so they are not echoed line-by-line here.
  });
  browser.on('Runtime.exceptionThrown', (p, sid) => {
    note();
    const d = p.exceptionDetails ?? {};
    const msg = d.exception?.description || d.text || 'exception';
    record(errors, `[${label(sid)}] uncaught: ${trunc(msg, 400)}`);
  });
  browser.on('Log.entryAdded', (p) => {
    const e = p.entry ?? {};
    if (e.level === 'error') record(errors, `[browser:${e.source}] ${trunc(e.text, 300)}`);
  });
  // A crash is a crash, named -- not a timeout to puzzle over later.
  const onCrash = (p, sid) => {
    crashed = `${label(sid)} crashed${p?.status ? ` (${p.status})` : ''} -- likely OOM`;
    record(errors, crashed);
  };
  browser.on('Target.targetCrashed', onCrash);
  browser.on('Inspector.targetCrashed', onCrash);

  // Flat auto-attach: every dedicated Worker gex spawns arrives here. Hold each
  // at start so not one of its earliest log lines is lost, then release it.
  browser.on('Target.attachedToTarget', async (p) => {
    const sid = p.sessionId;
    const info = p.targetInfo ?? {};
    if (info.type === 'worker' || info.type === 'shared_worker' || info.type === 'service_worker') {
      const name = shortWorker(info.url);
      workers.set(sid, name);
      log(`  worker + ${name}`);
      if (!o.quietWorkers) {
        try {
          await browser.send('Runtime.enable', {}, sid);
        } catch {
          /* the worker may already be gone */
        }
      }
    }
    if (p.waitingForDebugger) {
      try {
        await browser.send('Runtime.runIfWaitingForDebugger', {}, sid);
      } catch {
        /* ditto */
      }
    }
  });
  browser.on('Target.detachedFromTarget', (p) => {
    const name = workers.get(p.sessionId);
    if (name) {
      log(`  worker - ${name}`);
      workers.delete(p.sessionId);
    }
  });

  await browser.send('Page.enable', {}, page);
  await browser.send('Runtime.enable', {}, page);
  await browser.send('Log.enable', {}, page);
  await browser.send('Performance.enable', {}, page).catch(() => {});
  await browser.send(
    'Target.setAutoAttach',
    { autoAttach: true, waitForDebuggerOnStart: true, flatten: true },
    page,
  );
  // Capture gex:status into the page so the watch loop can read it without a
  // race, installed before any of the page's own script runs.
  await browser.send(
    'Page.addScriptToEvaluateOnNewDocument',
    {
      source:
        'window.__gex={statuses:[]};' +
        "addEventListener('gex:status',e=>window.__gex.statuses.push(e.detail));",
    },
    page,
  );

  await browser.send('Page.navigate', { url }, page);

  // --- the watch loop -------------------------------------------------------
  const began = Date.now();
  let printed = ''; // guest terminal already streamed
  let peakRss = 0;
  let warnedRss = false;
  let verdict = null;
  // --input state: dispatch synthetic mouse+keyboard on the compositor canvas
  // once the client is drawing, then require the shim's delivery trace.
  let framesSeenAt = null;
  let injectCount = 0;
  let lastInject = 0;
  let inputConfirmed = false;

  while (!verdict) {
    await sleep(250);
    const now = Date.now();

    if (crashed) {
      verdict = { how: 'crash', body: crashed };
      break;
    }
    if (now - began > o.wallMs) {
      verdict = { how: 'timeout', body: `wall of ${o.wallMs / 1000}s` };
      break;
    }
    if (now - lastActivity > o.idleMs) {
      verdict = {
        how: 'hang',
        body: `no terminal or console activity for ${Math.round((now - lastActivity) / 1000)}s`,
      };
      break;
    }

    let snap;
    try {
      // With a deadline, like the reporting below: `Runtime.evaluate` waits for
      // an answer from the page, and a page that has stopped answering never
      // sends one. Without this the watch loop parks inside the await and the
      // wall and idle guards below -- the whole point of the loop -- never run
      // again, so a wedged run reads as one still going.
      snap = await capped(evalPage(browser, page), 15_000, 'evaluate');
    } catch (e) {
      // A failed eval right after a navigation is normal; a persistent one on a
      // dead session is not, and the wall/crash guards above will end it.
      continue;
    }
    const { term, statuses } = snap;

    if (term !== printed) {
      streamTerminalDelta(printed, term);
      printed = term;
      note();
    }

    const rss = chromeRssMiB();
    if (rss > peakRss) peakRss = rss;
    // A single heads-up as it approaches trouble (a renderer OOM shows up as a
    // `Target.targetCrashed` above, but this is the warning before the kill).
    if (rss > 6144 && !warnedRss) {
      warnedRss = true;
      log(`  ⚠ chrome RSS ${rss} MiB -- approaching memory pressure`);
    }

    // --input: feed the compositor real DOM events on #gex-wl once it is
    // drawing, and only pass once its delivery trace confirms they reached a
    // client end to end (page ring -> poll -> compositor -> client).
    if (o.input) {
      // Start injecting once the app is plausibly up: a gears frame rate if it
      // prints one, otherwise a fixed cold-start margin (Mesa/Xwayland/the
      // client are all translated on first use, ~40-90s).
      if (
        framesSeenAt === null &&
        (/frames in [\d.]+ seconds/.test(term) || now - began > 45000)
      ) {
        framesSeenAt = now;
      }
      if (framesSeenAt !== null && injectCount < 40 && now - lastInject > 900) {
        lastInject = now;
        injectCount++;
        try {
          const r = await browser.send(
            'Runtime.evaluate',
            { expression: INJECT_JS, returnByValue: true },
            page,
          );
          if (injectCount === 1) log(`  input: ${r?.result?.value ?? '?'} on #gex-wl`);
        } catch {
          /* a transient eval failure just means we try again next tick */
        }
      }
      // The shim's delivery trace comes through the guest terminal (`term`),
      // not the console, so match there.
      inputConfirmed ||= /\[wl\] delivered \d+ input/.test(term);
      // The input goal is met while the client is still up: it is drawing
      // (frames) and the compositor's trace confirms an event reached it. Don't
      // wait for `want` (which needs the exit-time `lit=` line) -- that would
      // run the whole budget and read a delivered-input pass as a timeout.
      if (framesSeenAt !== null && inputConfirmed) {
        verdict = { how: 'pass', body: 'frames + input delivered to client' };
      }
    }

    // --applit: client-agnostic success. Any GL client that renders makes the
    // virgl renderer report lit>0, so poll its live state and pass on the first
    // lit frame -- no need for a client to print a frame rate or to exit.
    if (o.applit && !verdict) {
      try {
        const r = await browser.send(
          'Runtime.evaluate',
          { expression: 'globalThis.__gexGlState?.() ?? ""', returnByValue: true },
          page,
        );
        const gl = String(r?.result?.value ?? '');
        const lit = Number(/lit=(\d+)/.exec(gl.replace(/\s+/g, ''))?.[1] ?? 0);
        if (lit > 0) verdict = { how: 'pass', body: `rendered (lit=${lit})` };
      } catch {
        /* eval can fail transiently right after navigation */
      }
    }

    // --wllit: the universal Xwayland-pipeline success signal. Xwayland
    // composites every client -- GL or not -- to a wl_shm buffer the compositor
    // blits onto #gex-wl, so non-black pixels there mean the app drew and the
    // whole chain (client -> Xwayland -> gex-wayland -> canvas) worked.
    if (o.wllit && !verdict) {
      try {
        const r = await browser.send(
          'Runtime.evaluate',
          { expression: WLLIT_JS, returnByValue: true },
          page,
        );
        const lit = Number(r?.result?.value ?? 0);
        if (lit > 200) verdict = { how: 'pass', body: `compositor canvas lit (${lit}px)` };
      } catch {
        /* transient */
      }
    }

    const last = statuses.at(-1);
    // Goal reached?
    if (verdict) {
      // already decided (e.g. --input / --applit / --wllit above)
    } else if (matches(want, term)) {
      verdict = { how: 'pass', body: 'want matched' };
    } else if (matches(fail, term)) {
      verdict = { how: 'fail', body: 'fail pattern matched' };
    } else if (last?.kind === 'bad') {
      verdict = { how: 'fail', body: `status: ${last.text}` };
    } else if (settle && last?.text === settle) {
      // Reached the settle state but want did not match -- e.g. gears exited 0
      // having covered 0%. That is a real failure, not a pass.
      verdict = matches(want, term)
        ? { how: 'pass', body: 'want matched at settle' }
        : { how: 'fail', body: `settled '${settle}' without matching want` };
    } else {
      // A process exited without printing what was wanted: nothing more will
      // print, so stop rather than waiting out the wall.
      const gone = /\[[\w.+-]+ (?:exited|exit) (\d+)\]/.exec(term);
      if (gone && !matches(want, term)) {
        verdict = { how: 'fail', body: `guest exited ${gone[1]} without matching want` };
      }
    }
  }

  // --- report ---------------------------------------------------------------
  const secs = ((Date.now() - began) / 1000).toFixed(1);
  let shotPath = o.shot ?? join(STATE, `shot-${o.demo}.png`);
  try {
    const { data } = await capped(
      browser.send('Page.captureScreenshot', { format: 'png' }, page),
      10_000,
      'screenshot',
    );
    writeFileSync(shotPath, Buffer.from(data, 'base64'));
  } catch {
    shotPath = null;
  }

  const finalTerm = printed.replace(/\n+$/, '');
  console.log('\n---- guest terminal (tail) ----');
  console.log(
    finalTerm.split('\n').slice(-40).map((l) => `  | ${l}`).join('\n') || '  (empty)',
  );
  if (errors.length) {
    console.log(`\n---- ${errors.length} error(s) captured ----`);
    for (const e of errors.slice(0, 30)) console.log(`  ! ${e}`);
    if (errors.length > 30) console.log(`  ... and ${errors.length - 30} more`);
  }
  console.log(
    `\n---- ${verdict.how.toUpperCase()} site-${o.demo} in ${secs}s ` +
      `(peak chrome RSS ${peakRss} MiB${shotPath ? `, shot ${shotPath}` : ''}) ----`,
  );
  if (verdict.body) console.log(`     ${verdict.body}`);

  // A run that reached its goal but tripped an error still failed: a trapped
  // worker that happened to print the wanted line first is not a pass.
  let ok = verdict.how === 'pass';
  if (ok && errors.length && !o.allowErrors) {
    ok = false;
    console.log(`     (demoted: ${errors.length} error(s) despite the want matching)`);
  }

  if (!o.keep) {
    await capped(browser.send('Target.closeTarget', { targetId }), 10_000, 'close').catch(() => {});
  } else {
    log(`--keep: tab left open (targetId ${targetId}) on the persistent browser`);
  }
  browser.close();
  return ok ? 0 : 1;
}

// A CDP request with a deadline. Every command here awaits a reply from the
// page, and a *crashed* renderer never answers one: the socket stays open, so
// nothing rejects and nothing times out.
//
// That is not a detail. The reporting above runs after the verdict is decided,
// so a run that ended in "page crashed -- likely OOM" printed that line and
// then hung forever inside `Page.captureScreenshot`, with the verdict never
// printed, the exit code never returned, and -- the part that spread -- the tab
// never closed. Those tabs stay on the persistent browser and keep their
// memory, so the next run starts with less of it and crashes sooner. Two
// abandoned drivers were enough to turn a demo that finishes into one that OOMs
// halfway, and the evidence for the earlier, healthier runs was thrown away
// with them.
function capped(promise, ms, what) {
  let timer;
  return Promise.race([
    promise.finally(() => clearTimeout(timer)),
    new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(`cdp ${what} did not answer in ${ms}ms`)), ms);
    }),
  ]);
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------
function compileMatcher(s) {
  if (!s) return null;
  const m = /^\/(.*)\/([a-z]*)$/s.exec(s);
  return m ? new RegExp(m[1], m[2]) : new RegExp(s);
}
function matches(m, t) {
  return typeof m === 'function' ? !!m(t) : m instanceof RegExp ? m.test(t) : false;
}
function describe(m) {
  return typeof m === 'function' ? '(demo predicate)' : String(m ?? '(none)');
}
const MAX_ERRORS = 200;
function record(list, line) {
  if (list.length < MAX_ERRORS) list.push(line);
  console.log(`  ! ${trunc(line, 300)}`);
}
function previewArg(a) {
  if (a.value !== undefined) return String(a.value);
  if (a.description) return a.description;
  if (a.preview?.properties) return `{${a.preview.properties.map((p) => p.name).join(',')}}`;
  return a.type ?? '?';
}
const trunc = (s, n) => (s.length > n ? s.slice(0, n) + '…' : s);
function shortWorker(u) {
  if (!u) return 'worker';
  const m = /\/([^/?#]+)(?:[?#]|$)/.exec(u);
  return m ? m[1] : u.slice(0, 40);
}
async function evalPage(browser, page) {
  const { result } = await browser.send(
    'Runtime.evaluate',
    {
      expression:
        'JSON.stringify({term:(window.gexTerminalText?.()||""),statuses:(window.__gex?.statuses||[])})',
      returnByValue: true,
    },
    page,
  );
  return JSON.parse(result.value);
}
// Stream only what is new. xterm's whole-buffer text is prefix-stable while the
// output is small (every demo here); if an earlier line reflows we say so and
// reprint the tail rather than replay the lot.
function streamTerminalDelta(prev, next) {
  const p = prev.replace(/\n+$/, '');
  const n = next.replace(/\n+$/, '');
  if (n.startsWith(p)) {
    const added = n.slice(p.length).replace(/^\n/, '');
    for (const l of added.split('\n')) if (l !== '') console.log(`  > ${l}`);
  } else {
    console.log('  > (terminal reflowed)');
    for (const l of n.split('\n').slice(-4)) if (l !== '') console.log(`  > ${l}`);
  }
}

// ---------------------------------------------------------------------------
async function main() {
  const o = parseArgs(process.argv);
  if (!DEMOS[o.demo] && !o.want) {
    fatal(`unknown demo '${o.demo}' and no --want given (known: ${Object.keys(DEMOS).join(', ')})`);
  }
  if (o.build) {
    log('build: bash shims/browser/build.sh');
    const t = Date.now();
    const r = spawn('bash', [join(REPO, 'shims/browser/build.sh')], {
      cwd: REPO,
      stdio: 'inherit',
    });
    const code = await new Promise((res) => r.on('exit', res));
    if (code !== 0) fatal(`build failed (exit ${code})`);
    log(`build: ok in ${((Date.now() - t) / 1000).toFixed(0)}s`);
  }
  await ensureServer();
  const code = await runDemo(o);
  // Force exit: the persistent browser/server are detached and must stay up,
  // but this process's CDP socket and timers must not keep it alive.
  process.exit(code);
}

main().catch((e) => {
  console.error('drive: unexpected', e);
  process.exit(3);
});
