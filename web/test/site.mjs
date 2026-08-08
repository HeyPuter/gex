// The demo site itself, in a real browser.
//
//   node web/test/site.mjs [demo-id ...]
//
// Everything else here tests the runtime. This tests the *page*: the bundle
// vite built, the assets it ships, the wiring between a button and a guest.
// All of that has broken independently of the runtime -- a stale
// `gex_browser.wasm` missing an export the glue called, a rootfs tar that was
// never built, a demo that threw before it ran a byte -- and none of it showed
// up in any other test.
//
// The page is not modified to be testable. It gains two things a person can
// use as well: `?demo=<id>` picks a demo, `?run=1` presses its first button,
// and `status()` announces itself as a `gex:status` event. The reporter below
// is injected by this server into the served HTML, so what runs is the built
// bundle and nothing else.
import { spawn } from 'node:child_process';
import { readFile, access, stat } from 'node:fs/promises';
import { join, dirname, extname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createServer } from 'node:http';

const HERE = dirname(fileURLToPath(import.meta.url));
const WEB = join(HERE, '..');
const DIST = join(WEB, 'dist');
const CHROME = process.env.CHROME || '/usr/bin/chromium';
const WALL = Number(process.env.GEX_WALL || 900) * 1000;

// Which demos to drive, and what counts as having worked. A demo that needs a
// network or a GPU that this machine may not have says so rather than failing.
const DEMOS = {
  busybox: { want: /rootfs: \d+ files/, settle: 'running' },
  debian: { want: /rootfs: \d+ files/, settle: 'running' },
  gears: { want: /exit 0 in \d+ ms/, settle: 'finished' },
  virgl: { want: /graphics rootfs — \d+ files/, settle: 'running' },
};

const exists = async (p) => access(p).then(() => true, () => false);
const skip = (why) => {
  console.log(`skip: ${why}`);
  process.exit(0);
};

if (!(await exists(DIST))) skip(`no build at ${DIST} (npm run build)`);
if (!(await exists(CHROME))) skip(`no browser at ${CHROME}`);

const wanted = process.argv.slice(2).filter((a) => DEMOS[a]);
const list = wanted.length ? wanted : ['busybox', 'gears'];

const TYPES = {
  '.html': 'text/html',
  '.js': 'text/javascript',
  '.mjs': 'text/javascript',
  '.css': 'text/css',
  '.wasm': 'application/wasm',
  '.tar': 'application/x-tar',
};

// Injected into the page. It reports what the status line says and what the
// terminal holds, so a failure names the stage rather than the exit code.
const REPORTER = `
<script>
(() => {
  const post = (kind, body) =>
    fetch('/report?kind=' + kind, { method: 'POST', body }).catch(() => {});
  const term = () => document.querySelector('.xterm-rows')?.innerText || '';
  addEventListener('gex:status', (e) => {
    post('status', e.detail.kind + '|' + e.detail.text + '|' + term());
  });
  addEventListener('error', (e) => post('threw', String(e.message || e)));
  addEventListener('unhandledrejection', (e) =>
    post('threw', String(e.reason?.message || e.reason)));
})();
</script>`;

let onReport = () => {};
const server = createServer(async (req, res) => {
  if (req.url.startsWith('/report')) {
    const chunks = [];
    for await (const c of req) chunks.push(c);
    onReport(
      new URL(req.url, 'http://x').searchParams.get('kind'),
      Buffer.concat(chunks).toString(),
    );
    res.writeHead(204).end();
    return;
  }
  const name = (req.url === '/' ? '/index.html' : req.url).split('?')[0];
  const path = join(DIST, name);
  try {
    if (!(await stat(path)).isFile()) throw new Error('not a file');
    let body = await readFile(path);
    if (name === '/index.html') body = Buffer.from(body.toString() + REPORTER);
    res.writeHead(200, { 'content-type': TYPES[extname(name)] || 'application/octet-stream' });
    res.end(body);
  } catch {
    res.writeHead(404).end();
  }
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const port = server.address().port;

let failures = 0;
for (const id of list) {
  const { want, settle } = DEMOS[id];
  const url = `http://127.0.0.1:${port}/?demo=${id}&run=1`;
  let last = '';
  const done = new Promise((resolve) => {
    onReport = (kind, body) => {
      if (kind === 'threw') return resolve({ how: 'threw', body });
      const [statusKind, text, terminal] = body.split('|');
      last = `${statusKind || '-'} ${text}`;
      // Each status change is a line, so a run that takes minutes still says
      // where it is rather than going quiet.
      console.log(`     [${id}] ${text}`);
      if (want.test(terminal || '') || text === settle) {
        resolve({ how: 'reached', body: text });
      }
      if (statusKind === 'bad') resolve({ how: 'bad', body: text });
    };
  });

  const child = spawn(
    CHROME,
    ['--headless', '--no-sandbox', '--disable-gpu-sandbox', '--enable-unsafe-swiftshader', url],
    { stdio: ['ignore', 'ignore', 'pipe'] },
  );
  let err = '';
  child.stderr.on('data', (d) => (err += d));

  const result = await Promise.race([
    done,
    new Promise((r) => setTimeout(() => r({ how: 'timeout' }), WALL)),
  ]);
  child.kill('SIGKILL');
  onReport = () => {};

  if (result.how === 'reached') {
    console.log(`ok   site-${id}`);
  } else {
    failures++;
    console.log(`FAIL site-${id}: ${result.how}${result.body ? ` (${result.body})` : ''}`);
    console.log(`     last status: ${last || '(none)'}`);
    console.log(
      err.split('\n').filter((l) => l && !/Fontconfig|GPU|gpu|Registration/.test(l)).slice(-5)
        .map((l) => `     ${l}`).join('\n'),
    );
  }
}

server.close();
process.exit(failures ? 1 : 0);
