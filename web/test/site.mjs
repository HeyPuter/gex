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
  // The same demo, told to run `apt-get update` and exit rather than open a
  // shell. Needs a proxy: `GEX_QUERY='wisp=ws://127.0.0.1:5111/&cmd=apt-get
  // update'`, with `wisp-js-server` on that port.
  //
  // `Fetched` is the line apt prints when it has the index and has read it.
  // Not `Get:`, which only says a download started -- the run this exists to
  // catch got as far as one `Get:` and then stopped forever.
  // A command in the Debian rootfs, judged by a marker the command itself
  // prints. Not by `debian`'s prompt pattern: that is `/[#$]/`, which matches
  // a `$` anywhere -- including in the demo's own echo of the command it was
  // given. Several runs "passed" at the first tick on the strength of a `$?`
  // in the command line, and the browser was killed five seconds in, before
  // the guest had done anything. A check that can pass without the guest is
  // worse than no check.
  'debian-cmd': { demo: 'debian', want: /GEX-CMD-DONE/ },
  'debian-apt': {
    // The page has no such demo: this is the Debian one, run differently.
    demo: 'debian',
    want: /Fetched .* in |Reading package lists\.\.\. Done/,
    fail: /Temporary failure|Could not resolve|Failed to fetch/,
  },
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

// A stale `dist` is a test that proves nothing.
//
// `vite build` copies `web/public` into `web/dist`, so `dist` holds *another*
// copy of the runtime -- and this test served it without ever checking how old
// it was. The demo site ran for hours against a shim the harness had never
// loaded, and the harness stayed green throughout. Refusing to run is the
// least this can do about it.
if (!UPSTREAM) {
  const distShim = join(DIST, 'gex/gex_browser.wasm');
  const pubShim = fileURLToPath(new URL('../public/gex/gex_browser.wasm', import.meta.url));
  const [d, p] = await Promise.all([
    stat(distShim).catch(() => null),
    stat(pubShim).catch(() => null),
  ]);
  if (d && p && d.mtimeMs < p.mtimeMs) {
    skip(
      `${DIST} is older than the runtime it should contain `
      + `(dist ${new Date(d.mtimeMs).toISOString()}, `
      + `public ${new Date(p.mtimeMs).toISOString()}) -- run \`npm run build\`, `
      + 'or point this at a dev server with GEX_SITE=http://localhost:5173',
    );
  }
}

const wanted = process.argv.slice(2).filter((a) => DEMOS[a]);
// Debian by default, because that is the demo the site opens with and the one
// a person means by "the web demo". It was defined here and never run: the
// default set was busybox and gears, so every regression in the demo that
// matters most went unnoticed by the test that exists to catch them.
const list = wanted.length ? wanted : ['busybox', 'debian', 'gears'];

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
  // Decoded, because a URL path is not a file name.
  //
  // `libstdc++.so.6` reaches the server as `libstdc%2B%2B.so.6`: the closure
  // loader percent-encodes each component, as it must, and every real static
  // server decodes before it looks on disk. This one did not, so the first
  // closure the site ever tried to load 404'd on the third library.
  const raw = req.url.split('?')[0];
  const name = raw === '/' ? '/index.html' : decodeURIComponent(raw);
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
  const { settle, fail } = DEMOS[id];
  // `GEX_WANT=<regex>` says what the guest has to print, overriding the
  // demo's own pattern.
  //
  // `debian-cmd` passes on `GEX-CMD-DONE`, which the *shell* prints after the
  // command, so a run in which the command did entirely the wrong thing still
  // passes. That is not hypothetical: for hours `bash -c /bin/true` reported
  // a healthy 208-block run while the child was branching into bash's
  // translated code at `PIE_BASE` and only surviving because `/bin/true`
  // exits 0 whatever it executes. A command whose output is checked --
  // `GEX_WANT='^22$'` for `ls / | wc -l` -- cannot pass that way.
  const want = process.env.GEX_WANT ? new RegExp(process.env.GEX_WANT, 'm') : DEMOS[id].want;
  // A regular expression or a predicate. A demo whose evidence is two things
  // at once -- the guest drew *and* the frames reached the canvas -- cannot
  // say so in one pattern.
  const reached = typeof want === 'function' ? want : (t) => want.test(t);
  // `GEX_QUERY=gears_size=100x100` reaches the demo. An experiment that only
  // changes one thing about a run should not need a code change to run.
  const extra = process.env.GEX_QUERY ? `&${process.env.GEX_QUERY}` : '';
  const url = `http://127.0.0.1:${port}/?demo=${DEMOS[id].demo ?? id}&run=1${extra}`;
  // The command the page was told to run, decoded the way the page decodes
  // it, so the echo of it can be told apart from what the guest says. See
  // where `seen` is filled.
  //
  // `URLSearchParams` is what the page uses, and it turns `+` into a space --
  // so `i=$((i+1))` reaches the guest as `i=$((i 1))` and a shell loop
  // written with one never increments. Two repro runs "passed" that way
  // before this was noticed, which is worth knowing whenever a `?cmd=`
  // behaves as though it did nothing.
  const cmdText = new URLSearchParams(extra.slice(1)).get('cmd') || '';
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
      // Not `split('|')`. The terminal is the last field and the guest
      // puts pipes in it -- `echo x | cat` is a pipe on the command line,
      // and so is any program that draws a table -- so splitting on every
      // separator threw away everything the guest said after the first one.
      // The harness then judged the run on the truncated remainder, which is
      // how a pipeline could appear to produce no output at all.
      const first = body.indexOf('|');
      const second = body.indexOf('|', first + 1);
      const statusKind = body.slice(0, first);
      const text = body.slice(first + 1, second);
      const terminal = body.slice(second + 1);
      last = `${statusKind || '-'} ${text}`;
      // Kept so that a failure can show what the guest actually said. The
      // tick line below carries one line; a demo that fails is worth twenty.
      lastTerminal = terminal || lastTerminal;
      // The page's own echo of the command is not the guest saying anything.
      //
      // `bootDebian` prints `command: <cmd>` before it runs, and the history
      // below is what every `want` is tested against -- so a demo driven with
      // `?cmd=...; echo GEX-CMD-DONE` passes the instant the page prints the
      // command line, before the guest has run an instruction. Every
      // `debian-cmd` result taken that way is worthless, including two that
      // were being used to rule a crash out.
      //
      // This is the same defect the file already records for `debian`'s `$`
      // prompt, in a second place: a check that can pass without the guest is
      // worse than no check.
      // Matched by content rather than by position, because xterm wraps: the
      // command runs over several rows and every row after the first starts
      // at column zero, so there is no way to tell a continuation from output
      // by looking at the line alone. Every fragment of the echo *is* a
      // substring of the command, though, and nothing the guest prints has to
      // be.
      // Only the echo, and only until it ends. Filtering *every* line that
      // the command happens to contain throws away real output too: a command
      // ending `; echo GEX-CMD-DONE` contains the marker it exists to print,
      // so the run went from passing before the guest started to never
      // passing at all. Both are the same mistake -- deciding what a line
      // means by its text rather than by where it is.
      //
      // The echo begins at `command:` and continues while each row is a
      // fragment of the command; the first row that is not ends it, and
      // everything after counts. Recomputed per snapshot because the reporter
      // re-reads the whole terminal each tick.
      // Consumed positionally rather than by `includes`, so the echo ends
      // exactly where the command does. Matching anywhere swallowed the first
      // real line the guest printed whenever the command happened to contain
      // it -- `echo START` prints `START`, which is in the command, so it
      // vanished.
      let echoAt = -1;
      for (const line of (terminal || '').split('\n')) {
        const t = line.trim();
        if (/^command:/.test(t)) {
          echoAt = 0;
          continue;
        }
        if (echoAt >= 0 && echoAt < cmdText.length) {
          if (t && cmdText.startsWith(t, echoAt)) {
            echoAt += t.length;
            continue;
          }
          echoAt = cmdText.length;
        }
        if (!line.trim() || seen.has(line)) continue;
        seen.add(line);
        // `GEX_ECHO` prints every line the guest says, once, as it first
        // appears. The tick below carries one line per five seconds, which
        // is a progress indicator and not a transcript -- and a profile
        // printed by the guest is exactly the thing that scrolls past
        // between two ticks and is never seen.
        if (process.env.GEX_ECHO && new RegExp(process.env.GEX_ECHO).test(line)) {
          console.log(`     [${id}] ${line.trim()}`);
        }
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

  // `GEX_STDERR=all` on a *passing* run too.
  //
  // It was printed only on failure, which is backwards for the thing it is
  // most used for: the per-process timings the runtime logs to the console
  // are how a run is measured, and a measurement run is one that passes. A
  // three-`ls` loop completed, threw its own numbers away, and had to be run
  // again to get them.
  // On a failing run too, which is the whole point of asking for it.
  //
  // This said `&& result.how === 'reached'`, having been changed from "only
  // on failure" to "on a passing run as well" and landed on "only on a
  // passing run". So every run that timed out or died printed twenty lines of
  // terminal and nothing else -- no per-process timings, no `[gex]` trace, no
  // wasm stack -- and a whole session of debugging a deadlock was done by
  // reading the one row of the terminal that a tick happened to catch.
  const noisy = /Fontconfig|GPU|gpu|Registration|Vulkan|dawn/;
  if (process.env.GEX_STDERR === 'all') {
    for (const l of err.split('\n')) if (l && !noisy.test(l)) console.log(`     ${l}`);
  }
  if (result.how === 'reached') {
    // The terminal on a passing run too, when it was asked for. Everything
    // the *shim* narrates -- `trace_line`, and so every `[gex]`, `[closure]`
    // and `[link]` line -- is written to the guest's stderr and arrives here
    // rather than on the browser's console, and this printed it only when a
    // run failed. A diagnostic added to answer "did this fire at all" then
    // says nothing on the run where it fired, which cost two runs to notice.
    if (process.env.GEX_STDERR === 'all') {
      for (const line of lastTerminal.trim().split('\n').slice(-60)) {
        console.log(`     | ${line}`);
      }
    }
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
    // Eight lines of the browser's own stderr is enough to say a page threw
    // and never enough to say where. A wasm `RuntimeError` reaches the
    // console with the engine's stack under it -- the one thing that names
    // the function that trapped -- and it is tens of lines long, so the tail
    // that survived was always the part after the answer.
    //
    // `GEX_STDERR=all` keeps the lot. It is off by default because a passing
    // run has nothing worth reading here and a failing one usually does not
    // either; it is the crash in the runtime that needs it.
    const noise = /Fontconfig|GPU|gpu|Registration|Vulkan|dawn/;
    const lines = err.split('\n').filter((l) => l && !noise.test(l));
    console.log(
      (process.env.GEX_STDERR === 'all' ? lines : lines.slice(-8))
        .map((l) => `     ${l}`).join('\n'),
    );
  }
}

server.close();
process.exit(failures ? 1 : 0);
