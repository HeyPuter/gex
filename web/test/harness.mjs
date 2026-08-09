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
import { createServer } from 'node:http';
import { readFile, stat } from 'node:fs/promises';
import { join, dirname, extname, resolve as resolvePath } from 'node:path';
import { fileURLToPath } from 'node:url';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';

const HERE = dirname(fileURLToPath(import.meta.url));
const WEB = join(HERE, '..');
const DIST = join(WEB, 'dist');
const CHROME = process.env.CHROME || '/usr/bin/chromium';
const PORT = Number(process.env.GEX_HARNESS_PORT || 39900);

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

function makeServer(root) {
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
    if (p === '/control/run') {
      const cmd = url.searchParams.get('cmd') || '';
      const id = nextId++;
      const answer = new Promise((r) => pending.set(id, r));
      queue.push({ id, cmd, timeoutMs: Number(url.searchParams.get('timeout') || 120000) });
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

  const server = makeServer(root);
  await new Promise((r) => server.listen(PORT, '127.0.0.1', r));
  process.stderr.write(`harness: serving ${DIST} and ${root} on :${PORT}\n`);

  const profile = mkdtempSync(join(tmpdir(), 'gex-harness-'));
  const url =
    `http://127.0.0.1:${PORT}/?harness=1&shell=${encodeURIComponent(args.shell || '/bin/dash')}`;
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
    rmSync(profile, { recursive: true, force: true });
  };
  process.on('exit', bye);
  process.on('SIGINT', () => process.exit(0));
  process.on('SIGTERM', () => process.exit(0));
}

async function control(path, params = {}) {
  const q = new URLSearchParams(params).toString();
  const r = await fetch(`http://127.0.0.1:${PORT}${path}${q ? `?${q}` : ''}`);
  if (r.status === 204) return null;
  return r.json();
}

// ------------------------------------------------------------------- the CLI

const [verb, ...rest] = process.argv.slice(2);
const flags = {};
const positional = [];
for (let i = 0; i < rest.length; i++) {
  if (rest[i] === '--root') flags.root = rest[++i];
  else if (rest[i] === '--shell') flags.shell = rest[++i];
  else positional.push(rest[i]);
}

if (verb === 'start') {
  await start(flags);
} else if (verb === 'run') {
  const r = await control('/control/run', { cmd: positional.join(' ') });
  if (r.error) process.stderr.write(`harness: ${r.error}\n`);
  process.stdout.write(r.out || '');
  for (const l of r.log || []) process.stderr.write(`${l}\n`);
  process.stderr.write(`harness: ${r.ms}ms\n`);
  process.exit(r.error ? 1 : 0);
} else if (verb === 'ready') {
  console.log(JSON.stringify(await control('/control/ready')));
} else if (verb === 'stop') {
  await control('/control/stop').catch(() => {});
} else if (verb === 'once') {
  // Start, run each command in turn, stop. Still one browser for all of them.
  await start(flags);
  await control('/control/ready');
  for (const cmd of positional) {
    const r = await control('/control/run', { cmd });
    process.stdout.write(`--- ${cmd}\n${r.out || ''}`);
    for (const l of r.log || []) process.stderr.write(`${l}\n`);
    if (r.error) process.stderr.write(`harness: ${r.error}\n`);
    process.stderr.write(`harness: ${r.ms}ms\n`);
  }
  process.exit(0);
} else {
  console.error(
    'usage: harness.mjs start|run|ready|stop|once [--root DIR] [--shell PATH] [cmd...]',
  );
  process.exit(2);
}
