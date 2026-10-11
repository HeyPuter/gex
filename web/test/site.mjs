// The demo site in a real browser -- tests the *page* (vite bundle, assets,
// button->guest wiring), not the runtime.
//
//   node web/test/site.mjs [demo-id ...]
//
// The page gains `?demo=<id>`, `?run=1`, and a `gex:status` event; the
// REPORTER below is injected into the served HTML.
import { spawn } from 'node:child_process';
import { readFile, access, stat } from 'node:fs/promises';
import { join, dirname, extname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createServer } from 'node:http';
import { Cdp, browserWsUrl } from './cdp.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const WEB = join(HERE, '..');
// `GEX_DIST=web/dist-sp` tests the single-thread build's dist.
const DIST = process.env.GEX_DIST || join(WEB, 'dist');
const CHROME = process.env.CHROME || '/usr/bin/chromium';
// A site to test other than the built one; requests are passed through and
// only the HTML is touched, to add the reporter.
//
//   GEX_SITE=http://localhost:5173 node web/test/site.mjs gears
const UPSTREAM = process.env.GEX_SITE || '';
const WALL = Number(process.env.GEX_WALL || 900) * 1000;
// How many terminal lines to print; `GEX_TAIL=0` means all of the scrollback.
const TAIL = process.env.GEX_TAIL === undefined ? 60 : Number(process.env.GEX_TAIL);

// Which demos to drive, and what counts as having worked. Every `want` must be
// something the *guest* wrote, not a page line true of a demo that unpacked an
// image and did nothing. `settle` is the status that ends the wait; an
// interactive shell has none and must produce its prompt.
const DEMOS = {
  // Prompt patterns taken from a real run: xterm's `innerText` drops the
  // trailing space, so a pattern anchored on "$ " never matches.
  busybox: { want: /~ \$/ },
  // The Debian rootfs built from source (docs/gexcc.md §6): bash -l as root.
  'native-debian': { want: /:~#/ },
  // Typing into the terminal as a person does (CDP key events into xterm's
  // textarea, after the prompt): `$((1+1))` so the echo of the typed line
  // cannot pass for the answer.
  'native-debian-type': { demo: 'native-debian', ready: /:~#/, type: 'echo hi$((1+1))', want: /^hi2$/m },
  'busybox-type': { demo: 'busybox', ready: /~ \$/, type: 'echo hi$((1+1))', want: /^hi2$/m },
  'debian-type': { demo: 'debian', ready: /[#$]/, type: 'echo hi$((1+1))', want: /^hi2$/m },
  // Any command, from GEX_CMD, ended by `echo RUN=$((1+1))` (the echo of the
  // command holds no `RUN=2`).
  'native-debian-run': { demo: 'native-debian', want: /RUN=2/ },
  // First and second runs of a few programs, each timed in the guest: the
  // first pays the program's compile, the second what is left.
  'native-debian-cmd': {
    demo: 'native-debian',
    cmd:
      't(){ s=$(date +%s%N); "$@" > /dev/null 2>&1; r=$?; e=$(date +%s%N); echo "T $* rc=$r $(( (e-s)/1000000 ))ms"; }; '
      + 't true; t ls /; t ls /; t perl -e 1; t perl -e 1; '
      + 't dpkg -l; t dpkg -l; t apt-cache policy coreutils; t apt-cache policy coreutils; '
      + 'echo nd=done',
    // Guest lines only: the page's echo of the command wraps, and holds
    // `nd=done` too.
    want: /T apt-cache policy coreutils rc=0 \d+ms[\s\S]*T apt-cache policy coreutils rc=0 \d+ms/,
  },
  debian: { want: /[#$]/ },
  // A command in the Debian rootfs, judged by a marker the command prints, not
  // by `debian`'s `/[#$]/` prompt (which matches the page's echo of the command
  // -- a check that can pass without the guest is worse than no check).
  'debian-cmd': { demo: 'debian', want: /GEX-CMD-DONE/ },
  // The filesystem a package manager needs, in one shell line and no network:
  // an absolute symlink target must stay absolute, and a mode must survive a
  // rename (`test -x` follows the link and checks the mode).
  'debian-fs': {
    demo: 'debian',
    cmd:
      'd=$(mktemp -d); ln -s /bin/dash $d/l; test -x $d/l; echo link=$?; '
      + 'cp /bin/dash $d/n; chmod 755 $d/n; mv $d/n $d/m; test -x $d/m; echo mode=$?; '
      + 'echo fs=done',
    // `link=0`, not a literal marker: the page echoes the command, so a marker
    // in the command line would pass with no guest. `echo link=$?` cannot.
    want: /link=0[\s\S]*mode=0/,
  },
  'debian-apt': {
    demo: 'debian',
    want: /Fetched .* in |Reading package lists\.\.\. Done/,
    fail: /Temporary failure|Could not resolve|Failed to fetch/,
  },
  // dpkg-deb reading `.deb` archives carried in as base64 (no network). xz is
  // the interesting case: apt's downloads are all xz, and if it fails only in
  // the browser the fault is in Workers/fork/pipes rather than liblzma.
  'debian-deb': {
    demo: 'debian',
    cmd:
      'mkdir -p /var/tmp; echo ITxhcmNoPgpkZWJpYW4tYmluYXJ5LyAgMCAgICAgICAgICAgMCAgICAgMCAgICAgNjQ0ICAgICA0ICAgICAgICAgYAoyLjAKY29udHJvbC50YXIuZ3ovIDAgICAgICAgICAgIDAgICAgIDAgICAgIDY0NCAgICAgMTc4ICAgICAgIGAKH4sIAAAAAAAAA+3PsQrCMBSF4c55ijxCok0L3QRXwck9xKDB0Eqago9vLC4O4lRE/D+43OGc4V439DkNsVqSKpq6fmzdmpc908pU2qzWjTGq0apSWrelLtWiVz1NY7ZJyspFOx39+96n/EftrbvYk+9kFgefxjD0ndRik9w5ZO/ylEpkYxQ7G/pcxqdO3sTWjy6Fa57rWXz7CQAAAAAAAAAAAAAAAAD4Q3dAm9ZwACgAAGRhdGEudGFyLmd6LyAgICAwICAgICAgICAgICAwICAgICAwICAgICA2NDQgICAgIDEyMyAgICAgICBgCh+LCAAAAAAAAAPtzrEKwkAQhOGt8xT3CLvJ7fo8h55YHAjqvX8SsLEQSRFF+L9miplizrI/XUTOa9rBX/IpxHycwl3DVNTUzSTpF75Jvz/KLSU5ttJP9f3uU/+nLrW16/DrFwAAAAAAAAAAAAAAAACArWYUPsaHACgAAAo= | base64 -d > /var/tmp/g.deb; '
      + 'echo ITxhcmNoPgpkZWJpYW4tYmluYXJ5LyAgMCAgICAgICAgICAgMCAgICAgMCAgICAgNjQ0ICAgICA0ICAgICAgICAgYAoyLjAKY29udHJvbC50YXIueHovIDAgICAgICAgICAgIDAgICAgIDAgICAgIDY0NCAgICAgMjM2ICAgICAgIGAK/Td6WFoAAATm1rRGAgAhARYAAAB0L+Wj4Cf/AKpdADGbyhnbre+8GtxPrcaFT++rAjB/GkFNFgOm6VBpMA/Wk+GmL4DsY9923ZZlddS/nxFWoSR6utejD4OnYkiWbMXd2xl7yC6hji1I15Q1b0jslXWFAOutNxR5+KQM7Pm/Gt6oSbzHhH6BuMQY7N746W6unVxwT2G0kc/+tcp+PZFiI7SkqddI4q5rcH/QpyHQ4R52izaunn5LGqAzn3VPEzV/Mym0SdFzYToAAAAA3ulgv1Lac9MAAcYBgFAAAE+YYbixxGf7AgAAAAAEWVpkYXRhLnRhci54ei8gICAgMCAgICAgICAgICAgMCAgICAgMCAgICAgNjQ0ICAgICAxNzIgICAgICAgYAr9N3pYWgAABObWtEYCACEBFgAAAHQv5aPgJ/8AbF0AMwAzUBhoiYMB69bC9ZSe3K+9VyTKNDeVKuOE6q2pSZGdrE0ZtncO44v9udaLmMbY5oeEs6UeLUIVsAJqEJRT9SxkRLY485yHbIy8i5WG5laO+4Ibqg/4CQOkrEGMR1SXCVSino0b46KIte8AAJc7oVjYBFpmAAGIAYBQAABT8zONscRn+wIAAAAABFla | base64 -d > /var/tmp/x.deb; '
      + 'ls -l /var/tmp; '
      + 'dpkg-deb --info /var/tmp/g.deb > /dev/null; echo gzip=$?; '
      + 'dpkg-deb --info /var/tmp/x.deb > /dev/null; echo xz=$?; '
      // A control member that decompresses to 320 KB (5x the pipe's 65536
      // bytes), so the decompressing child must fill, block and resume -- which
      // the 600-byte archives above never test.
      + 'echo ITxhcmNoPgpkZWJpYW4tYmluYXJ5LyAgMCAgICAgICAgICAgMCAgICAgMCAgICAgNjQ0ICAgICA0ICAgICAgICAgYAoyLjAKY29udHJvbC50YXIueHovIDAgICAgICAgICAgIDAgICAgIDAgICAgIDY0NCAgICAgMzQ4ICAgICAgIGAK/Td6WFoAAATm1rRGAgAhARYAAAB0L+Wj5P//ARtdADGbyhnbre+8GtxPrcaFT++rAjB/GkFNFgOm6VBpMA/Wk+GmL4DsY9923ZZlddS/nxFWoSR6utejD4OnYkiWbMXd2xl7yC6hji1I15Q1b0jslXWFAOutNxR5+KQM7Pm/Gt6oSbzHhH6BuMQY7N746W6unVxwT2G0kc/+tcp+PZFiI7REUBLm94dVnFerpfTSgzVpceEv4oYua28TqSbkkII6ZLaZswE7ld59cWfaWUToepq872ywNUz4EcqGyzfX/steKt46lLisiZXrseHTYUfOEdeBfJHPhjX6fMK1RyGGR3x8iwPar0fM05MzKOJLuyFrt84iCjOxsWF6kNSIX8vMz0BOmNH7Uit0zR5bIEL5xtBvfhc6HQBsYAAAANz9em8slnU4AAG3AoCAFACe3picscRn+wIAAAAABFlaZGF0YS50YXIueHovICAgIDAgICAgICAgICAgIDAgICAgIDAgICAgIDY0NCAgICAgMTcyICAgICAgIGAK/Td6WFoAAATm1rRGAgAhARYAAAB0L+Wj4Cf/AGxdADMAM1AYaImDAevWwvWUntyvvVckyjQ3lSrjhOqtqUmRnaxNGbZ3DuOL/bnWi5jG2OaHhLOlHi1CFbACahCUU/UsZES2OPOch2yMvIuVhuZWjvuCG6oP+AkDpKxBjEdUlwlUop6NG+OiiLXvAACXO6FY2ARaZgABiAGAUAAAU/MzjbHEZ/sCAAAAAARZWg== | base64 -d > /var/tmp/b.deb; '
      + 'dpkg-deb --info /var/tmp/b.deb > /dev/null; echo big=$?; '
      // Through dpkg as apt reaches it: one more fork, a parent holding its
      // database and locks open; its per-package warnings go to /dev/null.
      + 'dpkg --unpack /var/tmp/x.deb > /dev/null 2>/dev/null; echo unpack=$?; '
      // The real thing: krb5-locales' own control member, byte-perfect in the
      // guest and fine under the native adapter -- 824 bytes in, 10 KiB out.
      + 'echo ITxhcmNoPgpkZWJpYW4tYmluYXJ5LyAgMCAgICAgICAgICAgMCAgICAgMCAgICAgNjQ0ICAgICA0ICAgICAgICAgYAoyLjAKY29udHJvbC50YXIueHovIDAgICAgICAgICAgIDAgICAgIDAgICAgIDY0NCAgICAgODI0ICAgICAgIGAK/Td6WFoAAATm1rRGBMD2BYBQIQEWAAAAAAAAANPeTiXgJ/8C7l0AFwu8HH0BlcAdSj55FcLMJqNjGNOjUET0IGysmtCfcXBMwWvX7Tk2J3yGcpaNT3YzR3VtUXAtNr3EwvvUHj6RAejWzT+bDi4VxhlhIOF/3nwcr45h86MrKpsLGvHgd32f/7BKFArxMiB4yCLKKwqhiOq51FP8gBqo51bJVcV7JkiPly2C6g7b5tsaiYXzsN1MXB3UH+ChbylXgE086VwmQqcndu/fNeKwz5aT23P5AdTTUlNrGqXY5x5rQnm3JyC3H9sk8l26FfB9IxnXVEnkXcs018kUTljQ38NCheJRV4IuDMJF581EZSgsSI13nONjelx8GnV8hnDGqJGLLKF3zZoGbzYsRz/46r3XoEVSBhqsFgFyiwTT/friCQgKuqKVD2ZP375jM1of6/3qVz5PGYNo9TGo8xOBWdz4pEma6KDPKeXFFzYGMjaYSdt0m5Kc5hHJ0nUzx66wsRe6JT5219quCwcrG7RJPvC2g8NOZU6jRbv6x4FzvUUcQQZxRnj93oaIWlmuN03vETi/n3iD/0O7pKUTCbwm4XX2/jQz42MUpQg9Dxgn5mgEbCejVhoe9RJ+Fo7GSDCGV3hiVqkQfQazeVxTkCgXpvND6abEe6mC4ROE5m5cUg67trbhEyV1OXqOUQQir8Cmjo4u/qcOC4DUo4yNuqvZ/5MzyxEb5ZTVNnwLxhzrHZEpiCuHFyjpeak62at7Ig83FhlyJjf356ayFKLOhOxHGPWAgTDGEivnAS3eNYj2j/en4d1EA0WkU3SADbRloUirkTq6Yu2ONuSEbQOwomSsxWtSRNp5a2dgHX+HXSqfkbks3DVUDCWcdyls0Bks6hvI+9lk7pjMOZo2Cn+1hCk4AkFSSEIHts9cZ9SxMZvkU0m9vTNGKqaHiLNioxYoBnDIcR3rxdGNn9b8Ou+htUbeqnwCkhIovMwrD7h9Md+GYAqzot3AyPmN8dzICL25ZrxFU45uK2rrWB4AuS791vOuygY07CsnAAAAERa3z0RiJLgAAZIGgFAAAL4OXp2xxGf7AgAAAAAEWVpkYXRhLnRhci54ei8gICAgMCAgICAgICAgICAgMCAgICAgMCAgICAgNjQ0ICAgICAxNzIgICAgICAgYAr9N3pYWgAABObWtEYCACEBFgAAAHQv5aPgJ/8AbF0AMwAzUBhoiYMB69bC9ZSe3K+9VyTKNDeVKuOE6q2pSZGdrE0ZtncO44v9udaLmMbY5oeEs6UeLUIVsAJqEJRT9SxkRLY485yHbIy8i5WG5laO+4Ibqg/4CQOkrEGMR1SXCVSino0b46KIte8AAJc7oVjYBFpmAAGIAYBQAABT8zONscRn+wIAAAAABFla | base64 -d > /var/tmp/r.deb; '
      + 'dpkg-deb --info /var/tmp/r.deb > /dev/null; echo real=$?; '
      // Bytes out of the decompressor, no tar in the way (`--ctrl-tarfile`
      // writes the tar to stdout): 10240 means liblzma worked and the fork is
      // at fault; 0 means it did not. The working archive is measured the same.
      + 'dpkg-deb --ctrl-tarfile /var/tmp/x.deb | wc -c; '
      + 'dpkg-deb --ctrl-tarfile /var/tmp/r.deb | wc -c; '
      + 'echo deb=done',
    want: /gzip=0[\s\S]*xz=0[\s\S]*big=0[\s\S]*real=0/,
  },
  // The same command twice: a second `ls` reuses what the first translated
  // (modules reach a process with slot numbers the *earlier* process chose),
  // so a dispatch change can be correct once and wrong the next run.
  'debian-twice': {
    demo: 'debian',
    cmd:
      'ls / > /dev/null; echo one=$?; ls / > /dev/null; echo two=$?; '
      + 'ls /usr > /dev/null; echo three=$?; echo twice=done',
    want: /one=0[\s\S]*two=0[\s\S]*three=0/,
  },
  // What a process costs when it does nothing: twenty `cat /dev/null`, so the
  // clock sees only process startup (Worker, shim instance, guest modules into
  // empty dispatch tables) -- the shape of the remaining cost in `apt-get update`.
  'debian-forks': {
    demo: 'debian',
    cmd:
      's=$(date +%s%N); i=0; '
      + 'while [ $i -lt 20 ]; do cat /dev/null; i=$((i+1)); done; '
      + 'e=$(date +%s%N); echo forks_ms=$(( (e-s)/1000000 )) n=$i; echo forks=done',
    want: /forks_ms=[0-9]+ n=20/,
  },
  // The control for `ld.so`: the same twenty forks from a static binary.
  // busybox is `static-pie`, so `busybox cat` runs no dynamic linker; the gap
  // from `debian-forks` is what dynamic linking costs. `cat /dev/null` not
  // `true`, so both sides open a file and the difference is only the linker.
  'busybox-forks': {
    demo: 'busybox',
    cmd:
      'i=0; while [ $i -lt 20 ]; do busybox cat /dev/null; i=$((i+1)); done; echo bbforks=done',
    want: /bbforks=done/,
  },
  // How fast the browser decompresses: `dpkg-deb` on a 2.4 MB archive (2.7s
  // under the native adapter). The only case here that measures guest CPU on
  // real work rather than correctness.
  'debian-lzma': {
    demo: 'debian',
    cmd:
      's=$(date +%s%N); dpkg-deb --fsys-tarfile /var/libc6.deb > /var/o.tar 2>/var/e.txt; '
      + 'rc=$?; e=$(date +%s%N); '
      + 'echo lzma_rc=$rc ms=$(( (e-s)/1000000 )) size=$(stat -c%s /var/o.tar); '
      + 'cat /var/e.txt; echo lzma=done',
    want: /lzma_rc=0 ms=[0-9]+ size=10373120/,
  },
  // Does a byte written come back? A 1.2 MB file copied through the shell and
  // checksummed both ways -- the shape of apt writing an archive dpkg can't read.
  'debian-io': {
    demo: 'debian',
    cmd:
      'a=$(md5sum < /bin/bash); cat /bin/bash > /var/tmp/b; '
      + 'b=$(md5sum < /var/tmp/b); echo A=$a; echo B=$b; '
      + 'test "$a" = "$b"; echo same=$?; '
      + 'wc -c < /bin/bash; wc -c < /var/tmp/b; echo io=done',
    want: /same=0/,
  },
  // Wisp throughput: the smallest thing that exercises the transport (Node host
  // ~3.8 MB/s vs browser 41.7 kB/s over the same syscalls). bash speaks
  // `/dev/tcp`, and a literal address because the guest resolver does not
  // answer bash -- removing DNS from a throughput measurement is right anyway.
  'debian-net': {
    demo: 'debian',
    cmd:
      's=$SECONDS; exec 3<>/dev/tcp/151.101.2.132/80; '
      + 'printf "GET /debian/dists/bookworm/main/binary-arm64/Packages.gz '
      + 'HTTP/1.0\\r\\nHost: deb.debian.org\\r\\n\\r\\n" >&3; '
      + 'n=$(cat <&3 | wc -c); echo bytes=$n secs=$((SECONDS-s)); echo net=done',
    want: /bytes=[0-9]{6,}/,
  },
  // `covered N%` is gles-gears' own glReadPixels count of non-background
  // pixels: a run that exits 0 having drawn a black frame (a GL path that ran
  // but presented nothing) must not pass, so the want is the coverage, not the
  // exit. `fail` catches the drew-nothing case explicitly.
  gears: {
    want: /covered ([1-9]\d*)%/,
    fail: /covered 0%/,
    settle: 'finished',
  },
};

const exists = async (p) => access(p).then(() => true, () => false);
const skip = (why) => {
  console.log(`skip: ${why}`);
  process.exit(0);
};

if (!UPSTREAM && !(await exists(DIST))) skip(`no build at ${DIST} (npm run build)`);
if (!(await exists(CHROME))) skip(`no browser at ${CHROME}`);

// A stale `dist` proves nothing: `vite build` copies `web/public` into `dist`,
// so refuse to run if the dist shim is older than the one in public.
if (!UPSTREAM) {
  const distShim = join(DIST, 'gex/gex_browser.shared.wasm');
  const pubShim = fileURLToPath(new URL('../public/gex/gex_browser.shared.wasm', import.meta.url));
  const [d, p] = await Promise.all([
    stat(distShim).catch(() => null),
    stat(pubShim).catch(() => null),
  ]);
  if (d && p && d.mtimeMs < p.mtimeMs) {
    skip(
      `${DIST} is older than the runtime it should contain `
      + `(dist ${new Date(d.mtimeMs).toISOString()}, `
      + `public ${new Date(p.mtimeMs).toISOString()}) -- run web/build-all.sh, `
      + 'or point this at a dev server with GEX_SITE=http://localhost:5173',
    );
  }
}

const wanted = process.argv.slice(2).filter((a) => DEMOS[a]);
// Debian included by default: it is the demo the site opens with.
const list = wanted.length ? wanted : ['busybox', 'debian', 'gears'];

const TYPES = {
  '.html': 'text/html',
  '.js': 'text/javascript',
  '.mjs': 'text/javascript',
  '.css': 'text/css',
  '.wasm': 'application/wasm',
  '.tar': 'application/x-tar',
};

// The reporter injected into the page, with `GEX_TYPE` baked in as JSON (a
// function, not a constant, because those lines contain quotes/`$`/backslashes).
const reporter = () =>
  REPORTER.replace(
    '%TYPE%',
    JSON.stringify((process.env.GEX_TYPE || '').split(';;').filter(Boolean)),
  );

const REPORTER = `
<script>
(() => {
  // debugexit=1 turns on the runtime's own narration of a fork: the
  // per-region checksums adoptProcess takes, and the step-by-step of
  // copying, instantiating and mapping the image.
  //
  // The page reads globalThis.GEX_DEBUG_EXIT and nothing ever set it, so the
  // one instrument that says whether a forked child's memory arrived intact
  // could be reached from Node and not from a browser -- which is where
  // forks actually break. This script is parsed before the module runs, so
  // the flag is there when runProgram looks.
  //
  // No backticks in this comment. It is inside a template literal, and one
  // backtick here ends the string and kills the harness at import -- which
  // the file warns about a hundred lines below and which I did anyway.
  if (location.search.includes('debugexit=1')) globalThis.GEX_DEBUG_EXIT = true;
  // ?recompile=N sets the object recompiler's budget in KiB; 0 turns it off.
  //
  // The page has it on at 8192 and the Node harness has it off by default,
  // and that difference was reachable from neither side at run time. It
  // decides whether a process translates a whole object in bulk after a few
  // misses, which is a different account of where a run's blocks come from
  // than the dispatch miss path. See task 136.
  {
    const m = /[?&]recompile=([0-9]+)/.exec(location.search);
    if (m) globalThis.GEX_RECOMPILE_OBJECTS = Number(m[1]);
  }
  // ?forkmods=0 stops a fork handing the child its parent's compiled
  // modules. The child then compiles what it branches into from the shim
  // registry it inherited in the image. See task 145: the list is unbounded,
  // reaches 3,227 in one shell, and is structured-cloned whole on every
  // fork, which is what puts the page into a V8 OOM at ~58 processes.
  {
    const m = /[?&]forkmods=([0-9]+)/.exec(location.search);
    if (m) globalThis.GEX_FORK_MODULES = Number(m[1]);
  }
  const post = (kind, body) =>
    fetch('/report?kind=' + kind, { method: 'POST', body }).catch(() => {});
  // The page's own reader gives the whole scrollback; the DOM query is the
  // viewport, which is the last two dozen rows of a run that scrolled.
  const term = () =>
    window.gexTerminalText?.() || document.querySelector('.xterm-rows')?.innerText || '';
  addEventListener('gex:status', (e) => {
    post('status', e.detail.kind + '|' + e.detail.text + '|' + term());
  });
  // Typing, for the shape a cmd= query cannot reach: an interactive shell.
  //
  // No backticks anywhere in here. This whole block lives inside a template
  // literal, so one backtick in a comment ends the string and the harness
  // dies at import with a syntax error pointing at prose -- which is exactly
  // what it did, twice, while the runs it was meant to drive reported
  // nothing at all.
  //
  // A cmd= query writes a script and runs it, which is a different program
  // from a person at a prompt: an interactive shell turns on job control --
  // setpgid, tcsetpgrp, a raw-mode line editor -- and a script turns on none
  // of them. The fault this exists for appears on the *second* command typed
  // at a prompt, and nothing here could type a first one.
  //
  // GEX_TYPE is lines separated by two semicolons, each sent once the
  // terminal has been quiet for a moment. Quiet rather than a prompt
  // pattern: prompts differ per shell and are often drawn with escapes.
  const toType = %TYPE%;
  if (toType.length) {
    let last = '';
    let still = 0;
    // Not before the demo is running. A demo spends its first minutes
    // fetching a rootfs and a closure, during which the terminal is quiet in
    // exactly the way this watches for -- so the first lines went into a page
    // with no guest behind it, and the run timed out having typed into
    // nothing.
    let live = false;
    addEventListener('gex:status', (e) => {
      if (e.detail.text === 'running') live = true;
    });
    const feed = setInterval(() => {
      if (!live) return;
      const now = term();
      still = now === last ? still + 1 : 0;
      last = now;
      // Three quiet ticks: the guest has stopped producing and is waiting.
      if (still < 3) return;
      const line = toType.shift();
      if (line === undefined) { clearInterval(feed); return; }
      still = 0;
      post('status', 'typing|' + line + '|');
      // Doubled, because this line is inside a template literal. Written
      // once, the backslash-r is consumed *here* and a raw carriage return
      // goes into the page inside a single-quoted string -- which no JS
      // string may contain, so the whole reporter died at parse with
      // "Invalid or unexpected token" and every run since reported
      // "last status: (none)". The typing feeder has therefore never run,
      // and neither has the status channel it shares a script with.
      window.gexType?.(line + '\\r');
    }, 700);
  }
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
  // Put the console level back into the message, because the browser throws
  // it away.
  //
  // Chrome logs every console level as INFO:CONSOLE on stderr -- 8,462 lines
  // of it in one run here, and not a single ERROR:CONSOLE even on runs where
  // a process trapped and console.error said so. So nothing downstream can
  // tell an error from progress chatter by severity, and the only reports
  // that were ever noticed were the ones a pattern happened to match.
  //
  // Marking the text survives the flattening, and covers EVERY console.error
  // rather than the call sites somebody remembered to hook -- which is the
  // difference between a net and a list.
  var realError = console.error.bind(console);
  // allowerrors=1 in the query logs console.error without ending the run --
  // needed to read a debug narration (GEX_DEBUG_EXIT prints per-step
  // console.error, which otherwise trips the fail-on-error net at the first
  // line). Default behaviour unchanged.
  var failOnError = location.search.indexOf('allowerrors') < 0;
  console.error = function () {
    var a = [].slice.call(arguments);
    if (failOnError) {
      try {
        post('threw', 'console.error: ' + a.map(String).join(' '));
      } catch (e) {
        // Reporting must never be what breaks the run.
      }
    }
    realError.apply(null, ['[gex-console-error]'].concat(a));
  };
  // A guest process dying, reported as an event rather than scraped.
  //
  // The runtime catches these -- the spawn promise is rejected and handled,
  // the reason written to the guest stderr -- so no error event fires and
  // nothing here would otherwise hear about it. Matching the prose was tried
  // and is the wrong mechanism: it fails OPEN, so rewording a message
  // disables the check silently, and the pattern ends up duplicated in the
  // places that read it and drifts. This is the runtime saying "a process
  // died", once, in a form that cannot be reworded away.
  //
  // No backticks anywhere in here: this whole reporter is a template
  // literal, and one backtick ends it.
  globalThis.GEX_ON_CRASH = (m) => post('threw', String(m));
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
  // An upstream server passed through; only the HTML is rewritten, to add the
  // reporter.
  if (UPSTREAM) {
    try {
      const target = new URL(req.url, UPSTREAM);
      const up = await fetch(target, { headers: { accept: req.headers.accept ?? '*/*' } });
      const type = up.headers.get('content-type') ?? 'application/octet-stream';
      const body = Buffer.from(await up.arrayBuffer());
      const html = type.includes('text/html');
      res.writeHead(up.status, {
        'content-type': type,
        // Added here so a dev server that forgets them still gets a
        // `SharedArrayBuffer` -- a different failure, worth telling apart.
        // Not with `GEX_NO_COI=1`, as for the built site below.
        ...(process.env.GEX_NO_COI ? {} : {
          'Cross-Origin-Opener-Policy': 'same-origin',
          'Cross-Origin-Embedder-Policy': 'require-corp',
        }),
      });
      res.end(html ? Buffer.from(body.toString() + reporter()) : body);
    } catch (e) {
      res.writeHead(502).end(String(e?.message ?? e));
    }
    return;
  }
  // Strip the query before matching the root, and decode: the closure loader
  // percent-encodes each path component (`libstdc++.so.6` arrives as
  // `libstdc%2B%2B.so.6`), so the path must be decoded before it hits disk.
  const raw = req.url.split('?')[0];
  const name = raw === '/' ? '/index.html' : decodeURIComponent(raw);
  const path = join(DIST, name);
  try {
    if (!(await stat(path)).isFile()) throw new Error('not a file');
    let body = await readFile(path);
    if (name === '/index.html') body = Buffer.from(body.toString() + reporter());
    // Cross-origin isolation: the page needs a `SharedArrayBuffer` for the
    // global kernel's memory, and without these headers it is not defined.
    // `GEX_NO_COI=1` leaves them off, as any static host would: the page then
    // has no SharedArrayBuffer and runs the single-thread build
    // (docs/single-thread.md).
    res.writeHead(200, {
      'content-type': TYPES[extname(name)] || 'application/octet-stream',
      ...(process.env.GEX_NO_COI ? {} : {
        'Cross-Origin-Opener-Policy': 'same-origin',
        'Cross-Origin-Embedder-Policy': 'require-corp',
      }),
    });
    res.end(body);
  } catch {
    res.writeHead(404).end();
  }
});
// `GEX_SITE_PORT`: a fixed port, so a second visit is the same origin (its OPFS).
await new Promise((r) => server.listen(Number(process.env.GEX_SITE_PORT ?? 0), '127.0.0.1', r));
const port = server.address().port;

let failures = 0;
for (const id of list) {
  const { settle, fail } = DEMOS[id];
  // `GEX_WANT=<regex>` overrides the demo's pattern with what the guest must
  // print -- e.g. `GEX_WANT='^22$'` for `ls / | wc -l`, which a marker printed
  // by the shell after the command cannot fake.
  const want = process.env.GEX_WANT ? new RegExp(process.env.GEX_WANT, 'm') : DEMOS[id].want;
  // A regex or a predicate (a demo whose evidence is two things at once cannot
  // say so in one pattern).
  const reached = typeof want === 'function' ? want : (t) => want.test(t);
  // `GEX_QUERY=gears_size=100x100` reaches the demo without a code change.
  const extra = process.env.GEX_QUERY ? `&${process.env.GEX_QUERY}` : '';
  // A demo may carry its own command (`debian-fs` runs one shell line);
  // `GEX_QUERY` still wins because it comes after. Encoded, since a query splits
  // on `&` and a command with `&&` or `2>&1` would arrive cut in half.
  const own = DEMOS[id].cmd ? `&cmd=${encodeURIComponent(DEMOS[id].cmd)}` : '';
  // `GEX_CMD=<shell line>` is the same from outside, encoded here so `&&`,
  // `2>&1` and `+` survive (`+` decodes to a space).
  const cmd = process.env.GEX_CMD
    ? `&cmd=${encodeURIComponent(process.env.GEX_CMD)}`
    : '';
  const url =
    `http://127.0.0.1:${port}/?demo=${DEMOS[id].demo ?? id}&run=1${own}${extra}${cmd}`;
  // The command decoded the way the page decodes it, so its echo can be told
  // apart from guest output. `URLSearchParams` turns `+` into a space, so
  // `i=$((i+1))` reaches the guest as `i=$((i 1))` -- watch for that.
  const cmdText = new URLSearchParams(extra.slice(1)).get('cmd') || '';
  const spec = DEMOS[id];
  let typed = false;
  // `GEX_TRACE=<file>`: a Chrome trace (every thread: V8's sampling profiler,
  // wasm compilation, GC) from the guest printing GEX_TRACE_BEGIN (a regex)
  // to its printing GEX_TRACE_END, written to <file> as JSON events.
  const traceFile = process.env.GEX_TRACE || '';
  const traceBegin = traceFile ? new RegExp(process.env.GEX_TRACE_BEGIN || 'TRACE-BEGIN') : null;
  const traceEnd = traceFile ? new RegExp(process.env.GEX_TRACE_END || 'TRACE-END') : null;
  let tracing = null;
  // A debugging port for the typing demos (the keys go in over CDP) and traces.
  const debugPort = spec.type || traceFile ? 39300 + Math.floor(Math.random() * 600) : 0;
  let last = '';
  let lastTerminal = '';
  // Every terminal line ever shown, in first-appearance order: the reporter
  // reads only the on-screen rows, and a scrolling run loses evidence far
  // apart in time (glxgears' frame rate while it runs, pixel counts after).
  const seen = new Set();
  const history = () => [...seen].join('\n');
  const done = new Promise((resolve) => {
    onReport = (kind, body) => {
      if (kind === 'threw') return resolve({ how: 'threw', body });
      // Not `split('|')`: the terminal is the last field and the guest puts
      // pipes in it, so only the first two separators are boundaries.
      const first = body.indexOf('|');
      const second = body.indexOf('|', first + 1);
      const statusKind = body.slice(0, first);
      const text = body.slice(first + 1, second);
      const terminal = body.slice(second + 1);
      last = `${statusKind || '-'} ${text}`;
      lastTerminal = terminal || lastTerminal;
      // Skip the page's echo of the command (`bootDebian` prints `command:
      // <cmd>` before running), so a `?cmd=...; echo MARKER` demo cannot pass
      // on the echoed marker before the guest runs. Consumed positionally --
      // xterm wraps, so the echo is matched by walking `cmdText`, and it ends
      // exactly where the command does; everything after counts. Recomputed
      // per snapshot because the reporter re-reads the whole terminal.
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
        // `GEX_ECHO=<regex>` prints every matching guest line once, as a full
        // transcript -- the tick below is one line per five seconds, not one.
        if (process.env.GEX_ECHO && new RegExp(process.env.GEX_ECHO).test(line)) {
          console.log(`     [${id}] ${line.trim()}`);
        }
      }
      // One line per status change so a slow run still shows progress; a tick
      // also carries the terminal's last line.
      const tail = (terminal || '').trim().split('\n').filter(Boolean).slice(-1)[0] || '';
      console.log(`     [${id}] ${text}${statusKind === 'tick' && tail ? ` | ${tail}` : ''}`);
      if (traceFile && !tracing && traceBegin.test(history())) {
        tracing = startTrace(debugPort).catch((e) => (console.log(`     [${id}] trace: ${e?.message ?? e}`), null));
      }
      if (tracing && traceEnd.test(history()) && !tracing.ending) {
        tracing.ending = true;
        tracing.then((t) => t?.stop(traceFile)).then((n) => n !== undefined && console.log(`     [${id}] trace: ${n} events in ${traceFile}`));
      }
      if (spec.type && !typed && spec.ready.test(history())) {
        typed = true;
        typeInto(debugPort, spec.type).then(
          (how) => console.log(`     [${id}] typed ${JSON.stringify(spec.type)} + Enter (${how})`),
          (e) => resolve({ how: 'bad', body: `typing failed: ${e?.message ?? e}` }),
        );
      }
      if (reached(history()) || text === settle) {
        resolve({ how: 'reached', body: text });
      }
      // A run that proved the opposite, so the wait need not run to the wall.
      const failed = typeof fail === 'function' ? fail : (t) => fail?.test(t);
      if (fail && failed(terminal || '')) {
        resolve({ how: 'bad', body: 'nothing reached the canvas' });
      }
      if (statusKind === 'bad') resolve({ how: 'bad', body: text });
      // The guest process exited without printing what was wanted -- nothing
      // more can print, so stop rather than waiting out the wall. After the
      // `reached` test above, so a run that printed it and *then* exited passes.
      const gone = /\[[\w.+-]+ exited (\d+)\]/.exec(terminal || '');
      if (gone) {
        resolve({ how: 'bad', body: `the guest exited ${gone[1]} without printing what was wanted` });
      }
    };
  });

  // Wall time from launching the browser, for `ok` lines (startup numbers).
  const t0 = Date.now();
  const child = spawn(
    CHROME,
    [
      '--headless',
      '--no-sandbox',
      '--disable-gpu-sandbox',
      '--enable-unsafe-swiftshader',
      // GEX_WEBGPU_SOFTWARE=1: WebGPU on SwiftShader, as drive.mjs does, so
      // gears runs on a host with no GPU.
      ...(process.env.GEX_WEBGPU_SOFTWARE === '1'
        ? ['--enable-unsafe-webgpu', '--use-angle=swiftshader-webgl', '--use-webgpu-adapter=swiftshader']
        : []),
      // Without this the page's console goes nowhere, so a demo that throws
      // before running a byte fails as "timeout, last status: (none)".
      '--enable-logging=stderr',
      '--v=0',
      // `performance.memory` exists only behind this -- the only way to measure
      // the JS heap from a Worker here, and the only account of the V8 OOM (#145).
      '--enable-precise-memory-info',
      ...(debugPort ? [`--remote-debugging-port=${debugPort}`, '--remote-allow-origins=*'] : []),
      // `GEX_USER_DATA_DIR=<dir>`: a profile that outlives this launch, so a
      // second run is a second visit (OPFS kept: `?xcache=1`).
      ...(process.env.GEX_USER_DATA_DIR ? [`--user-data-dir=${process.env.GEX_USER_DATA_DIR}`] : []),
      url,
    ],
    { stdio: ['ignore', 'ignore', 'pipe'] },
  );
  // Never leave the browser behind: a run that throws (a trace too big to
  // write, say) once left one holding the profile, and the next launch on
  // that profile handed its URL to the old browser.
  process.on('exit', () => child.kill('SIGKILL'));
  let err = '';
  child.stderr.on('data', (d) => (err += d));

  // `let`, because a crash found in the console below demotes a "reached".
  let result = await Promise.race([
    done,
    new Promise((r) => setTimeout(() => r({ how: 'timeout' }), WALL)),
  ]);
  if (tracing) {
    // Let a trace that is being written finish first.
    const t = await tracing;
    if (t && !t.stopped) {
      const n = await t.stop(traceFile).catch(() => 0);
      console.log(`     [${id}] trace: ${n} events in ${traceFile}`);
    }
    await t?.done;
  }
  child.kill('SIGKILL');
  onReport = () => {};

  // `GEX_STDERR=all` prints the browser's stderr on both passing and failing
  // runs (the per-process timings are wanted on a measurement run, which passes).
  const noisy = /Fontconfig|GPU|gpu|Registration|Vulkan|dawn/;
  if (process.env.GEX_STDERR === 'all') {
    for (const l of err.split('\n')) if (l && !noisy.test(l)) console.log(`     ${l}`);
  }
  // Every console error, listed in full and fatal even when the wanted text
  // appeared: the `[gex-console-error]` marker is added by the reporter and
  // each Worker because Chrome logs all levels as INFO. A trapped process has
  // not passed.
  const consoleErrors = err
    .split('\n')
    .filter((l) => l.includes('[gex-console-error]'));
  if (consoleErrors.length) {
    console.log(`     ${consoleErrors.length} console error(s):`);
    for (const l of consoleErrors) console.log(`     ! ${l.trim()}`);
    if (result.how === 'reached') {
      result = {
        how: 'bad',
        body: `${consoleErrors.length} console error(s), first: ${consoleErrors[0].trim().slice(0, 300)}`,
      };
    }
  }
  if (result.how === 'reached') {
    // The terminal on a passing run too, when asked for: the shim's `[gex]`,
    // `[closure]` and `[link]` narration goes to the guest's stderr, here.
    if (process.env.GEX_STDERR === 'all') {
      for (const line of (TAIL ? lastTerminal.trim().split('\n').slice(-TAIL) : lastTerminal.trim().split('\n'))) {
        console.log(`     | ${line}`);
      }
    }
    console.log(`ok   site-${id} (${Date.now() - t0} ms from browser launch)`);
  } else {
    failures++;
    // The tail of the terminal, not just its last line: what a demo printed on
    // its way to failing is the only account of why.
    for (const line of (TAIL ? lastTerminal.trim().split('\n').slice(-TAIL) : lastTerminal.trim().split('\n'))) {
      console.log(`     | ${line}`);
    }
    console.log(`FAIL site-${id}: ${result.how}${result.body ? ` (${result.body})` : ''}`);
    console.log(`     last status: ${last || '(none)'}`);
    // Last eight lines of the browser's stderr by default; `GEX_STDERR=all`
    // keeps the lot, which a wasm `RuntimeError`'s long engine stack needs.
    const noise = /Fontconfig|GPU|gpu|Registration|Vulkan|dawn/;
    const lines = err.split('\n').filter((l) => l && !noise.test(l));
    console.log(
      (process.env.GEX_STDERR === 'all' ? lines : lines.slice(-8))
        .map((l) => `     ${l}`).join('\n'),
    );
  }
}

// `GEX_TRACE`: start a browser-wide trace; `stop(file)` ends it and writes
// the events (a JSON array, as chrome://tracing reads it), giving their count.
async function startTrace(port) {
  let ws = null;
  for (let i = 0; i < 40 && !ws; i++) {
    ws = await browserWsUrl(port);
    if (!ws) await new Promise((r) => setTimeout(r, 250));
  }
  if (!ws) throw new Error(`no browser on debugging port ${port}`);
  const b = await Cdp.connect(ws);
  const events = [];
  b.on('Tracing.dataCollected', (p) => { for (const e of p.value) events.push(e); });
  let finished;
  const complete = new Promise((r) => (finished = r));
  b.on('Tracing.tracingComplete', () => finished());
  await b.send('Tracing.start', {
    transferMode: 'ReportEvents',
    traceConfig: {
      recordMode: 'recordContinuously',
      includedCategories: [
        // Not devtools.timeline, toplevel or v8.execute: a task event per
        // message slowed node -e 0 by two thirds.
        ...(process.env.GEX_TRACE_CATEGORIES?.split(',') ?? ['v8', 'v8.wasm', 'blink.worker', 'disabled-by-default-v8.cpu_profiler']),
      ],
    },
  });
  console.log('     trace: started');
  const t = {
    stopped: false,
    done: null,
    async stop(file) {
      if (t.stopped) return t.done;
      t.stopped = true;
      t.done = (async () => {
        await b.send('Tracing.end');
        await complete;
        // One event per line: a long trace is past V8's longest string.
        const { createWriteStream } = await import('node:fs');
        const out = createWriteStream(file);
        out.write('[\n');
        for (let i = 0; i < events.length; i++) {
          if (!out.write(`${JSON.stringify(events[i])}${i + 1 < events.length ? ',' : ''}\n`)) {
            await new Promise((r) => out.once('drain', r));
          }
        }
        out.write(']\n');
        await new Promise((r) => out.end(r));
        b.close();
        return events.length;
      })();
      return t.done;
    },
  };
  return t;
}

// Focus xterm's textarea and type `text` then Enter, the way a key press
// arrives (Input.insertText is what an IME or a paste produces; the Enter is
// a real keyDown/keyUp pair).
async function typeInto(port, text) {
  const ws = await browserWsUrl(port);
  if (!ws) throw new Error(`no browser on debugging port ${port}`);
  const b = await Cdp.connect(ws);
  const { targetInfos } = await b.send('Target.getTargets');
  const t = targetInfos.find((x) => x.type === 'page');
  if (!t) throw new Error('no page target');
  const { sessionId } = await b.send('Target.attachToTarget', { targetId: t.targetId, flatten: true });
  const focus = await b.send('Runtime.evaluate', {
    expression: `(() => { const t = document.querySelector('.xterm-helper-textarea'); if (!t) return 'no xterm textarea'; t.focus(); return document.activeElement === t ? 'focused' : 'not focused'; })()`,
    returnByValue: true,
  }, sessionId);
  for (const ch of text) await b.send('Input.insertText', { text: ch }, sessionId);
  const enter = { key: 'Enter', code: 'Enter', windowsVirtualKeyCode: 13, nativeVirtualKeyCode: 13 };
  await b.send('Input.dispatchKeyEvent', { type: 'keyDown', text: '\r', ...enter }, sessionId);
  await b.send('Input.dispatchKeyEvent', { type: 'keyUp', ...enter }, sessionId);
  // What the kernel's terminal holds afterwards: bytes typed and not read
  // say the shell is not reading; none and no echo say the keys never came.
  await new Promise((r) => setTimeout(r, 3000));
  const tty = await b.send('Runtime.evaluate', {
    expression: `(() => { const t = globalThis.gexTty; return t ? 'tty ready=' + t.ready() + ' lflag=0x' + t.lflag().toString(16) : 'no shared tty'; })()`,
    returnByValue: true,
  }, sessionId);
  b.close();
  return `${focus.result?.value}; ${tty.result?.value}`;
}

server.close();
process.exit(failures ? 1 : 0);
