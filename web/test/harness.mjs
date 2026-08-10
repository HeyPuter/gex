// A browser that stays up, so a measurement costs what the guest costs.
//
//   node web/test/harness.mjs start [--root DIR] [--shell /bin/dash]
//   node web/test/harness.mjs run 'ls /bin | wc -l'
//   node web/test/harness.mjs stop
//
// `start` runs in the foreground and holds the browser open; `run` and `stop`
// talk to it over a control port. Or in one shot:
//
//   node web/test/harness.mjs once 'ls /bin' 'ls /bin' 'ls /bin'
//
// Why this exists. `web/test/site.mjs` boots Chromium, serves the bundle,
// fetches a rootfs tar and unpacks it into the guest filesystem -- and then
// the demo runs one command and exits. Three `ls` measured that way cost 295
// seconds against 20 under Node, and nearly all of the difference is setup
// that is identical every time. Ask the same question twice and you pay for
// Chromium twice.
//
// So the page boots once and answers commands over HTTP (see
// `web/src/harness.js`), and the rootfs is streamed from a real directory
// rather than a built image -- the same tree `run-node.mjs --dir` reads, so
// the two hosts are measured against identical bytes and a changed file needs
// no rebuild.
import { spawn } from 'node:child_process';
import { createServer, request as httpRequest } from 'node:http';
import { readFile, readdir, stat } from 'node:fs/promises';
import { join, dirname, basename, extname, resolve as resolvePath } from 'node:path';
import { fileURLToPath } from 'node:url';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';

const HERE = dirname(fileURLToPath(import.meta.url));
const WEB = join(HERE, '..');
const DIST = join(WEB, 'dist');
const CHROME = process.env.CHROME || '/usr/bin/chromium';
const PORT = Number(process.env.GEX_HARNESS_PORT || 39900);

// The kernel's memory is a SharedArrayBuffer, and a page only gets one when it
// is cross-origin isolated. Every response the page reads needs these, not
// only the document: without them the harness silently runs a one-process
// world, which is not what anything here wants to measure.
const ISOLATED = {
  'cross-origin-opener-policy': 'same-origin',
  'cross-origin-embedder-policy': 'require-corp',
  'cross-origin-resource-policy': 'cross-origin',
};

const TYPES = {
  '.html': 'text/html',
  '.js': 'text/javascript',
  '.mjs': 'text/javascript',
  '.css': 'text/css',
  '.wasm': 'application/wasm',
  '.json': 'application/json',
  '.tar': 'application/x-tar',
};

// ---------------------------------------------------------------- the server

async function serveStatic(req, res) {
  const url = new URL(req.url, 'http://x');
  let path = url.pathname === '/' ? '/index.html' : url.pathname;
  const file = join(DIST, path);
  try {
    const body = await readFile(file);
    res.writeHead(200, {
      'content-type': TYPES[extname(file)] || 'application/octet-stream',
      // The kernel's memory is a SharedArrayBuffer, and a page only gets one
      // when it is cross-origin isolated. Without these two headers the
      // harness silently runs a one-process world, which is not what anything
      // here wants to measure.
      'cross-origin-opener-policy': 'same-origin',
      'cross-origin-embedder-policy': 'require-corp',
      'cross-origin-resource-policy': 'cross-origin',
    });
    res.end(body);
  } catch {
    res.writeHead(404).end('not found');
  }
}

/** The rootfs, tarred straight out of a real directory. */
function streamRootfs(root, res) {
  res.writeHead(200, {
    'content-type': 'application/x-tar',
    'cross-origin-opener-policy': 'same-origin',
    'cross-origin-embedder-policy': 'require-corp',
    'cross-origin-resource-policy': 'cross-origin',
  });
  // `-h` so symlinks arrive as the files they point at: the guest filesystem
  // holds bytes, and a dangling link in a real rootfs -- Debian has several --
  // must not stop the stream.
  const tar = spawn('tar', ['-C', root, '-cf', '-', '.'], {
    stdio: ['ignore', 'pipe', 'ignore'],
  });
  tar.stdout.pipe(res);
  res.on('close', () => tar.kill('SIGKILL'));
}

function body(req) {
  return new Promise((r) => {
    let s = '';
    req.on('data', (c) => (s += c));
    req.on('end', () => r(s));
  });
}

/**
 * A closure build, as the page needs it.
 *
 * `gexc build --closure -o <stem>.wasm` writes the image module at
 * `<stem>.wasm`, one `<stem>.NN.<name>.wasm` per library beside it, and
 * `<stem>.pins` saying where each object was placed. `run-node.mjs` reads all
 * of that off the disk through `GEX_CLOSURE`; a page cannot, so the server
 * hands over a manifest and the files.
 */
async function closureManifest(stem) {
  const dir = dirname(stem);
  const base = basename(stem);
  const libs = (await readdir(dir))
    .filter((n) => n.startsWith(`${base}.`) && n.endsWith('.wasm') && n !== `${base}.wasm`)
    .sort();
  // `base span path` per line, the same three fields `gexc run --pin` takes.
  const pins = {};
  const text = await readFile(`${stem}.pins`, 'utf8').catch(() => '');
  for (const line of text.split('\n')) {
    const f = line.split(' ');
    if (f.length < 3) continue;
    const at = Number(f[0]);
    if (!Number.isFinite(at) || at <= 0) continue;
    pins[f.slice(2).join(' ').trim()] = { base: at, span: Number(f[1]) || 0 };
  }
  return { image: `${base}.wasm`, libs, pins };
}

function makeServer(root, closureStem) {
  // Commands waiting for the page, and the HTTP responses waiting for their
  // results.
  const queue = [];
  let waitingPage = null;
  const pending = new Map();
  let ready = null;
  const readyWaiters = [];
  let nextId = 1;

  const handOut = () => {
    if (!waitingPage || !queue.length) return;
    const job = queue.shift();
    const res = waitingPage;
    waitingPage = null;
    res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(job));
  };

  const server = createServer(async (req, res) => {
    const url = new URL(req.url, 'http://x');
    const p = url.pathname;

    if (p === '/harness/rootfs.tar') return streamRootfs(root, res);

    // The closure: what it is made of, and then each piece by name.
    if (p === '/harness/closure.json') {
      if (!closureStem) return res.writeHead(404).end('no closure');
      const m = await closureManifest(closureStem);
      return res
        .writeHead(200, { 'content-type': 'application/json', ...ISOLATED })
        .end(JSON.stringify(m));
    }
    if (p.startsWith('/harness/closure/')) {
      // The name comes from the manifest this server wrote, but it arrives
      // over the wire: anything with a separator in it is not ours.
      const name = decodeURIComponent(p.slice('/harness/closure/'.length));
      if (!closureStem || name.includes('/') || name.includes('..')) {
        return res.writeHead(404).end('not found');
      }
      const bytes = await readFile(join(dirname(closureStem), name)).catch(() => null);
      if (!bytes) return res.writeHead(404).end('not found');
      return res
        .writeHead(200, { 'content-type': 'application/wasm', ...ISOLATED })
        .end(bytes);
    }

    if (p === '/harness/ready') {
      ready = JSON.parse((await body(req)) || '{}');
      for (const w of readyWaiters.splice(0)) w(ready);
      return res.writeHead(204).end();
    }

    // The page asking for work. Held open until there is some, so a command
    // starts the instant it is enqueued.
    if (p === '/harness/next') {
      waitingPage = res;
      const t = setTimeout(() => {
        if (waitingPage === res) {
          waitingPage = null;
          res.writeHead(204).end();
        }
      }, 25000);
      res.on('close', () => clearTimeout(t));
      return handOut();
    }

    if (p === '/harness/result') {
      const r = JSON.parse((await body(req)) || '{}');
      const waiter = pending.get(r.id);
      if (waiter) {
        pending.delete(r.id);
        waiter(r);
      } else if (r.fatal) {
        process.stderr.write(`harness: the page died: ${r.fatal}\n`);
      }
      return res.writeHead(204).end();
    }

    // The driver asking for a command to be run. Held open until the page
    // answers, so `run` is one request and no polling.
    // Pick up a rebuilt bundle without losing the browser.
    //
    // A new shim means the page has to fetch `dist` again, and the way that
    // was done was to stop the whole run and start another -- a fresh
    // Chromium, a fresh rootfs unpack, thirty seconds before the first
    // command. The browser is the expensive part and none of it needs
    // replacing: the page reloads, re-reads the bundle, boots a new shell,
    // and says it is ready again.
    if (p === '/control/reload') {
      ready = null;
      queue.push({ id: nextId++, reload: true });
      handOut();
      return res.writeHead(204).end();
    }

    if (p === '/control/run') {
      const cmd = url.searchParams.get('cmd') || '';
      const id = nextId++;
      const answer = new Promise((r) => pending.set(id, r));
      // Fifteen minutes by default. Two was chosen for `ls`; `apt-get update`
      // takes nearly two on its own under Node and more in a browser, and a
      // measurement that gives up before the guest does is not a measurement.
      queue.push({ id, cmd, timeoutMs: Number(url.searchParams.get('timeout') || 900000) });
      handOut();
      const r = await answer;
      return res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(r));
    }

    if (p === '/control/ready') {
      if (ready) {
        return res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(ready));
      }
      const r = await new Promise((w) => readyWaiters.push(w));
      return res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(r));
    }

    if (p === '/control/stop') {
      res.writeHead(204).end();
      setTimeout(() => process.exit(0), 50);
      return;
    }

    return serveStatic(req, res);
  });
  return server;
}

// ---------------------------------------------------------------- the driver

async function start(args) {
  const root = args.root || '/tmp/apt-full';
  await stat(join(DIST, 'index.html')).catch(() => {
    console.error(`harness: no built site at ${DIST}. Run 'npm run build' in web/.`);
    process.exit(2);
  });
  await stat(root).catch(() => {
    console.error(`harness: no rootfs at ${root}`);
    process.exit(2);
  });

  const server = makeServer(root, args.closure ? resolvePath(args.closure) : null);
  await new Promise((r) => server.listen(PORT, '127.0.0.1', r));
  process.stderr.write(`harness: serving ${DIST} and ${root} on :${PORT}\n`);

  const profile = mkdtempSync(join(tmpdir(), 'gex-harness-'));
  const url =
    `http://127.0.0.1:${PORT}/?harness=1&shell=${encodeURIComponent(args.shell || '/bin/dash')}` +
    (args.trace ? '&trace=1' : '') +
    (args.debug ? '&debug=1' : '') +
    (args.adopt ? `&adopt=${encodeURIComponent(args.adopt)}` : '') +
    (args.wisp ? `&wisp=${encodeURIComponent(args.wisp)}` : '') +
    (args.closure ? '&closure=1' : '') +
    (args.program ? `&program=${encodeURIComponent(args.program)}` : '') +
    // The one-shot path awaits the guest and nothing else, so without a
    // deadline a wedged program never reports and the run has to be found and
    // killed by hand. Three closure attempts cost ten minutes each that way,
    // and `--wait` looked like it was being ignored because on that path it
    // was: it reached `/control/run`, which the one-shot page never asks.
    (args.wait ? `&wait=${encodeURIComponent(args.wait)}` : '') +
    (args.progArgs || []).map((a) => `&arg=${encodeURIComponent(a)}`).join('') +
    (args.recompile !== undefined ? `&recompile=${encodeURIComponent(args.recompile)}` : '');
  const chrome = spawn(
    CHROME,
    [
      '--headless=new',
      '--no-sandbox',
      '--disable-gpu',
      `--user-data-dir=${profile}`,
      // The page needs a SharedArrayBuffer; the headers are served above, and
      // this keeps a bare IP origin from being treated as insecure.
      '--enable-features=SharedArrayBuffer',
      '--enable-logging=stderr',
      '--v=0',
      url,
    ],
    { stdio: ['ignore', 'ignore', 'pipe'] },
  );
  chrome.stderr.on('data', (d) => {
    const s = String(d);
    // The page's own console, which is where the per-process translation
    // counters come out.
    for (const line of s.split('\n')) {
      if (line.includes('CONSOLE') || line.includes('ERROR:')) process.stderr.write(`${line}\n`);
    }
  });
  const bye = () => {
    chrome.kill('SIGKILL');
    // The kill has been sent, not waited for, and Chromium writes its profile
    // out as it goes: the directory can gain a file between the walk and the
    // unlink, which is `ENOTEMPTY` -- and thrown from an `exit` handler that
    // turns a finished run into a crash. A leftover directory in the
    // temporary space is not worth reporting an error over.
    try {
      rmSync(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 50 });
    } catch {
      /* the run is over; a stale profile directory is not a failure */
    }
  };
  process.on('exit', bye);
  process.on('SIGINT', () => process.exit(0));
  process.on('SIGTERM', () => process.exit(0));
}

/**
 * Ask the server, and wait as long as it takes.
 *
 * `fetch` will not do: the control endpoints are held open until the page has
 * an answer, and undici gives up on a response whose headers have not
 * arrived within its own timeout. A command that runs for minutes -- which is
 * every interesting one, `apt-get update` among them -- died with
 * `UND_ERR_HEADERS_TIMEOUT` before the guest had finished. `http.request` has
 * no such limit.
 */
function control(path, params = {}) {
  const q = new URLSearchParams(params).toString();
  return new Promise((resolve, reject) => {
    const req = httpRequest(
      { host: '127.0.0.1', port: PORT, path: `${path}${q ? `?${q}` : ''}` },
      (res) => {
        let body = '';
        res.setEncoding('utf8');
        res.on('data', (c) => (body += c));
        res.on('end', () => resolve(res.statusCode === 204 ? null : JSON.parse(body || 'null')));
      },
    );
    req.on('error', reject);
    // No socket timeout at all: the answer arrives when the guest is done.
    req.setTimeout(0);
    req.end();
  });
}

// ------------------------------------------------------------------- the CLI

const [verb, ...rest] = process.argv.slice(2);
const flags = {};
const positional = [];
for (let i = 0; i < rest.length; i++) {
  if (rest[i] === '--root') flags.root = rest[++i];
  else if (rest[i] === '--shell') flags.shell = rest[++i];
  else if (rest[i] === '--trace') flags.trace = true;
  else if (rest[i] === '--debug') flags.debug = true;
  else if (rest[i] === '--wisp') flags.wisp = rest[++i];
  // How long to let one command run before giving up on it and
  // returning whatever it printed. The default is fifteen minutes, which
  // is right for a slow-but-working `apt-get update` and exactly wrong
  // for chasing a hang: every iteration cost the full fifteen before
  // saying anything. The output collected so far comes back either way.
  else if (rest[i] === '--wait') flags.wait = rest[++i];
  else if (rest[i] === '--adopt') flags.adopt = rest[++i];
  // One program instead of a shell, which is the only way to measure a
  // closure: its pins are a layout for that program and collide with a
  // shell's own placement.
  else if (rest[i] === '--program') flags.program = rest[++i];
  else if (rest[i] === '--closure') flags.closure = rest[++i];
  else if (rest[i] === '--recompile') flags.recompile = rest[++i];
  else positional.push(rest[i]);
}

if (verb === 'start') {
  await start(flags);
} else if (verb === 'run') {
  const r = await control('/control/run', {
    cmd: positional.join(' '),
    ...(flags.wait ? { timeout: flags.wait } : {}),
  });
  if (r.error) process.stderr.write(`harness: ${r.error}\n`);
  process.stdout.write(r.out || '');
  for (const l of r.log || []) process.stderr.write(`${l}\n`);
  process.stderr.write(`harness: ${r.ms}ms\n`);
  process.exit(r.error ? 1 : 0);
} else if (verb === 'reload') {
  await control('/control/reload');
  // And wait for the page to come back up, so the next `run` is not sent
  // into a browser that is still unpacking.
  console.log(JSON.stringify(await control('/control/ready')));
} else if (verb === 'program') {
  flags.progArgs = positional;
  await start(flags);
  const r = await control('/control/run', { cmd: '', ...(flags.wait ? { timeout: flags.wait } : {}) });
  process.stdout.write(r.out || '');
  for (const l of r.log || []) process.stderr.write(`${l}\n`);
  if (r.error) process.stderr.write(`harness: ${r.error}\n`);
  process.stderr.write(`harness: exit ${r.status}, ${r.ms}ms\n`);
  process.exit(r.status === 0 ? 0 : 1);
} else if (verb === 'ready') {
  console.log(JSON.stringify(await control('/control/ready')));
} else if (verb === 'stop') {
  await control('/control/stop').catch(() => {});
} else if (verb === 'once') {
  // Start, run each command in turn, stop. Still one browser for all of them.
  await start(flags);
  await control('/control/ready');
  for (const cmd of positional) {
    const r = await control('/control/run', {
      cmd,
      ...(flags.wait ? { timeout: flags.wait } : {}),
    });
    process.stdout.write(`--- ${cmd}\n${r.out || ''}`);
    for (const l of r.log || []) process.stderr.write(`${l}\n`);
    if (r.error) process.stderr.write(`harness: ${r.error}\n`);
    process.stderr.write(`harness: ${r.ms}ms\n`);
  }
  process.exit(0);
} else {
  console.error(
    'usage: harness.mjs start|run|ready|reload|stop|once [--root DIR] [--shell PATH]\n' +
      '                   [--trace] [--debug] [--recompile KIB] [--wisp URL]\n' +
      '                   [--closure STEM] [--wait MS] [cmd...]',
  );
  process.exit(2);
}
