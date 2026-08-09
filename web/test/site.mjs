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
// A site to test other than the built one.
//
//   GEX_SITE=http://localhost:5173 node web/test/site.mjs virgl
//
// `dist` and `npm run dev` are not the same page: the dev server serves the
// module graph rather than a bundle, transforms on the fly, and starts its
// Workers from source. Everything here had only ever been proved against
// `dist`, which is the one nobody develops against.
//
// The requests are passed through so the page under test is exactly what that
// server sends; only the HTML is touched, to add the reporter.
const UPSTREAM = process.env.GEX_SITE || '';
const WALL = Number(process.env.GEX_WALL || 900) * 1000;

// Which demos to drive, and what counts as having worked. A demo that needs a
// network or a GPU that this machine may not have says so rather than failing.
// Every `want` is something the *guest* wrote. The page's own lines --
// "rootfs: N files", a status of running -- were what these used to look for,
// and both are true of a demo that unpacks an image and then does nothing at
// all. That is how every one of these passed while no guest ran: the Worker
// chunk imported a module that did not exist, 404'd, and rejected where
// nobody was listening.
//
// `settle` is the status that ends the wait when there is one. An interactive
// shell never finishes, so those demos have no settle and must produce their
// prompt.
const DEMOS = {
  // The prompt each shell actually prints, taken from a run rather than
  // guessed: xterm's `innerText` drops the trailing space, so a pattern
  // anchored on "$ " never matches a prompt that is plainly there.
  busybox: { want: /~ \$/ },
  debian: { want: /[#$]/ },
  gears: { want: /exit 0 in \d+ ms/, settle: 'finished' },
  // The guest's own evidence, not the page's. "graphics rootfs -- N files"
  // and a status of `running` were both true of a demo that unpacked the
  // image and then did nothing at all, which is how this test passed while
  // the demo did not boot.
  //
  // Two things have to be true and only one of them is about the guest.
  // glxgears' frame rate says Mesa completed frames and got them back, but
  // vtest has no present -- the X server reads its pixels back and never
  // looks at a screen -- so a run can draw perfectly onto a canvas that
  // stays black. `presented`, which the demo prints when the run ends, is
  // the number of blits that actually reached it.
  virgl: {
    // The count is read from the terminal with its wrapping taken out.
    // xterm breaks a line at the window width and `innerText` keeps the
    // break, so a summary wide enough to wrap splits `presented=7` across
    // two rows and a pattern that spans it never matches -- a run that drew
    // and presented sat there until the wall clock.
    // `lit`, not `presented`. A blit happens whether or not there was
    // anything in the frame, so `presented` counts arrivals at the canvas and
    // says nothing about what arrived -- which is how a run that drew nothing
    // at all passed while the canvas was black. `lit` counts the blits whose
    // source texture had a pixel in it.
    want: (t) =>
      /frames in [\d.]+ seconds/.test(t) &&
      Number(/lit=(\d+)/.exec(t.replace(/\s+/g, ''))?.[1]) > 0,
    // No `settle`: a run that finishes having presented nothing is the exact
    // failure this is for, and a status of `finished` would call it a pass.
    fail: (t) => /lit=0(?!\d)/.test(t.replace(/\s+/g, '')),
  },
};

const exists = async (p) => access(p).then(() => true, () => false);
const skip = (why) => {
  console.log(`skip: ${why}`);
  process.exit(0);
};

if (!UPSTREAM && !(await exists(DIST))) skip(`no build at ${DIST} (npm run build)`);
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
  // A ping the moment this runs, so "nothing arrived" can be told from
  // "the page never loaded". They are the same output otherwise.
  post('status', 'reporter|installed|');
  // And one every few seconds, carrying the terminal.
  //
  // A status change is not enough: a demo that reaches the running state and
  // then stops changes nothing again, so the last thing reported is the last
  // thing before the interesting part. That is exactly the failure this file
  // is for, and it went unseen: the check passed the moment the status said
  // the demo was running, while the demo did nothing at all.
  let ticks = 0;
  setInterval(() => post('status', 'tick|running ' + ++ticks * 5 + 's|' + term()), 5000);
  // The message, or -- when there is none because this is a *resource*
  // failing rather than code throwing -- what failed to load. Reporting the
  // event itself stringifies to "[object Event]", which names nothing.
  addEventListener('error', (e) => {
    const t = e.target;
    if (t && t !== window) {
      post('threw', 'failed to load ' + (t.src || t.href || t.tagName));
    } else {
      post('threw', String(e.message || e));
    }
  });
  // In the capture phase as well, which is the only way a *resource* load
  // failure is heard -- a module script that 404s or is served as the wrong
  // type fires on the element, not on window, and bubbles nowhere. That is
  // invisible to every listener above and is what a page that loads and
  // then does absolutely nothing looks like.
  addEventListener(
    'error',
    (e) => {
      const t = e.target;
      if (t && t !== window) {
        post('threw', 'failed to load ' + (t.src || t.href || t.tagName));
      }
    },
    true,
  );
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
  // A server of somebody else's, passed through.
  //
  // Only the HTML is rewritten, and only to add the reporter: what runs has
  // to be that server's page, or this proves nothing about it.
  if (UPSTREAM) {
    try {
      const target = new URL(req.url, UPSTREAM);
      const up = await fetch(target, { headers: { accept: req.headers.accept ?? '*/*' } });
      const type = up.headers.get('content-type') ?? 'application/octet-stream';
      const body = Buffer.from(await up.arrayBuffer());
      const html = type.includes('text/html');
      res.writeHead(up.status, {
        'content-type': type,
        // Added here rather than assumed: a dev server that forgets them
        // has no `SharedArrayBuffer`, which is a different failure and one
        // worth telling apart from this one.
        'Cross-Origin-Opener-Policy': 'same-origin',
        'Cross-Origin-Embedder-Policy': 'require-corp',
      });
      res.end(html ? Buffer.from(body.toString() + REPORTER) : body);
    } catch (e) {
      res.writeHead(502).end(String(e?.message ?? e));
    }
    return;
  }
  // The query goes first, then the check for the root. The other order
  // compared `/?demo=busybox&run=1` against `/`, did not match, stripped the
  // query afterwards and asked for the directory -- which is not a file, so
  // the server answered 404 to its own page. Every demo then "timed out with
  // no status", which is exactly what a page that was never served does.
  const name = req.url.split('?')[0] === '/' ? '/index.html' : req.url.split('?')[0];
  const path = join(DIST, name);
  try {
    if (!(await stat(path)).isFile()) throw new Error('not a file');
    let body = await readFile(path);
    if (name === '/index.html') body = Buffer.from(body.toString() + REPORTER);
    // Cross-origin isolation, as the dev server and the deployed site send.
    // The page needs a `SharedArrayBuffer` for the global kernel's memory,
    // and without these headers the constructor is not there at all.
    res.writeHead(200, {
      'content-type': TYPES[extname(name)] || 'application/octet-stream',
      'Cross-Origin-Opener-Policy': 'same-origin',
      'Cross-Origin-Embedder-Policy': 'require-corp',
    });
    res.end(body);
  } catch {
    res.writeHead(404).end();
  }
});
await new Promise((r) => server.listen(0, '127.0.0.1', r));
const port = server.address().port;

let failures = 0;
for (const id of list) {
  const { want, settle, fail } = DEMOS[id];
  // A regular expression or a predicate. A demo whose evidence is two things
  // at once -- the guest drew *and* the frames reached the canvas -- cannot
  // say so in one pattern.
  const reached = typeof want === 'function' ? want : (t) => want.test(t);
  // `GEX_QUERY=gears_size=100x100` reaches the demo. An experiment that only
  // changes one thing about a run should not need a code change to run.
  const extra = process.env.GEX_QUERY ? `&${process.env.GEX_QUERY}` : '';
  const url = `http://127.0.0.1:${port}/?demo=${id}&run=1${extra}`;
  let last = '';
  let lastTerminal = '';
  // Every line the terminal has ever shown, in the order it first appeared.
  //
  // The reporter can only read the rows on screen, and a run long enough to
  // scroll loses the top of what it said. That is fatal for evidence made of
  // two things far apart in time: glxgears prints its frame rate while it
  // runs and its pixel counts after it exits, and by the time the counts
  // arrive the frame rates are gone. A snapshot holds one or the other and
  // never both, so a run that had proved everything sat until the wall.
  const seen = new Set();
  const history = () => [...seen].join('\n');
  const done = new Promise((resolve) => {
    onReport = (kind, body) => {
      if (kind === 'threw') return resolve({ how: 'threw', body });
      const [statusKind, text, terminal] = body.split('|');
      last = `${statusKind || '-'} ${text}`;
      // Kept so that a failure can show what the guest actually said. The
      // tick line below carries one line; a demo that fails is worth twenty.
      lastTerminal = terminal || lastTerminal;
      for (const line of (terminal || '').split('\n')) {
        if (line.trim()) seen.add(line);
      }
      // Each status change is a line, so a run that takes minutes still says
      // where it is rather than going quiet. A tick carries the terminal's
      // last line as well, because that is where the guest speaks.
      const tail = (terminal || '').trim().split('\n').filter(Boolean).slice(-1)[0] || '';
      console.log(`     [${id}] ${text}${statusKind === 'tick' && tail ? ` | ${tail}` : ''}`);
      if (reached(history()) || text === settle) {
        resolve({ how: 'reached', body: text });
      }
      // A run that ended having proved the opposite. Without this the wait
      // runs to the wall clock, and fifteen minutes of nothing reads as a
      // hang rather than as the answer it already has.
      const failed = typeof fail === 'function' ? fail : (t) => fail?.test(t);
      if (fail && failed(terminal || '')) {
        resolve({ how: 'bad', body: 'nothing reached the canvas' });
      }
      if (statusKind === 'bad') resolve({ how: 'bad', body: text });
    };
  });

  const child = spawn(
    CHROME,
    [
      '--headless',
      '--no-sandbox',
      '--disable-gpu-sandbox',
      '--enable-unsafe-swiftshader',
      // Without this the page's console goes nowhere, and a demo that throws
      // before it runs a byte fails as "timeout, last status: (none)" with
      // nothing else to say. That is the failure this file exists to catch.
      '--enable-logging=stderr',
      '--v=0',
      url,
    ],
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
    // The end of the terminal, not just its last line: what a demo printed on
    // its way to failing is the only account of why.
    for (const line of lastTerminal.trim().split('\n').slice(-20)) {
      console.log(`     | ${line}`);
    }
    console.log(`FAIL site-${id}: ${result.how}${result.body ? ` (${result.body})` : ''}`);
    console.log(`     last status: ${last || '(none)'}`);
    console.log(
      err.split('\n').filter((l) => l && !/Fontconfig|GPU|gpu|Registration|Vulkan|dawn/.test(l))
        .slice(-8)
        .map((l) => `     ${l}`).join('\n'),
    );
  }
}

server.close();
process.exit(failures ? 1 : 0);
