// dlopen in a static program (fixtures/probe/rt-dlstatic.c): a program with
// its libc inside it loads a side module (rt-side.c) whose imports, libc's
// among them, are the program's exports, from a lazy file, fetched as
// dlopen reads it, found by its bare name beside the program (a static
// program's run path is its own directory); calls into it from two threads, each calling back into
// the program's libc; its constructor, its data and a second dlopen.
import { runProgram } from '../../page/src/run.js';
import { setGlobals } from '../../page/src/page-globals.js';
import { untar } from '../../page/src/tar.js';
import { unzstd } from '../../page/src/zfetch.js';
import { bytesOf } from '../../page/runtime/src/internal.js';

export const meta = { hosts: ['browser', 'node'], images: ['busybox'], timeout: 180 };

export default async function (t) {
  const { say, check, eq, timeout } = t;
  const dbg = t.debug();
  const sw = Object.fromEntries(Object.entries(dbg).filter(([k]) => k !== 'arch'));
  globalThis.GEX_PAGE_SEARCH = `?${new URLSearchParams(sw)}`;
  globalThis.GEX_ASSET_BASE = t.baseUrl;
  setGlobals((n) => sw[n] ?? null, { arch: dbg.arch ?? 'aarch64' });
  const tree = untar(await unzstd(await bytesOf(t.work('bb.tar.zst'))));
  const side = await bytesOf(t.work('rt-side.so'));
  const sideUrl = new URL(t.work('rt-side.so'));
  const sideAt = sideUrl.protocol === 'file:' ? decodeURIComponent(sideUrl.pathname) : sideUrl.pathname;
  let out = '';
  const term = {
    rows: 24, cols: 80,
    write: (s) => { out += typeof s === 'string' ? s : new TextDecoder().decode(s); },
    writeln: (s) => { out += `${s}\n`; },
    onData: () => ({ dispose() {} }),
    onResize: () => ({ dispose() {} }),
    focus() {},
  };
  const t0 = performance.now();
  const run = runProgram(term, {
    filesystem: 'memory',
    elf: tree.files['/bin/rt-dlstatic'],
    args: ['/bin/rt-dlstatic', 'rt-side.so'],
    files: tree.files,
    dirs: tree.dirs ?? [],
    modes: tree.modes,
    hard: tree.hard,
    symlinks: tree.links ?? {},
    lazy: [['/bin/rt-side.so', side.length, sideAt]],
    env: ['PATH=/bin:/usr/bin', 'HOME=/root'],
    cwd: '/',
  });
  run.catch((e) => { out += `\n[runProgram threw: ${e?.stack ?? e}]`; });
  const r = await timeout(run, 120000, 'the end').catch((e) => { throw new Error(`${e.message}: ${out.slice(-600)}`); });
  const lines = out.replace(/\r/g, '');
  say(lines.split('\n').map((l) => `  | ${l}`).join('\n'));
  say(`the program ran in ${(performance.now() - t0).toFixed(0)} ms`);
  const ms = lines.match(/^dlopen ([0-9.]+) ms$/m);
  if (ms) say(`dlopen of a ${side.length}-byte side module, its fetch and compile: ${ms[1]} ms`);
  eq('the program\'s status', r.status, 0);
  check('dlopen in a static program', !!ms, lines.slice(0, 300));
  check('the side module called back into the program\'s fprintf', /^side: hello$/m.test(lines));
  check('every check passed', /^ok$/m.test(lines) && !/^FAIL/m.test(lines), lines.slice(0, 600));
}
