// runProgram (page/src/run.js) as a preset over the runtime API
// (run-program.js): an untarred bundle laid into the root with its hard
// links, symbolic links, modes, extended attributes, file times and a lazy
// file; the program, by a directory link's name, a child of the kernel's pid 1, on a terminal bridged to
// an xterm-shaped object; keys typed into it; its exit status; and its last
// output before the system stops.
import { runProgram } from '../../page/src/run.js';
import { setGlobals } from '../../page/src/page-globals.js';
import { untar } from '../../page/src/tar.js';
import { unzstd } from '../../page/src/zfetch.js';
import { bytesOf } from '../../page/runtime/src/internal.js';

export const meta = { hosts: ['browser', 'node'], images: ['busybox'], timeout: 180 };

export default async function (t) {
  const { say, check, eq, until, timeout } = t;
  const dbg = t.debug();
  const sw = Object.fromEntries(Object.entries(dbg).filter(([k]) => k !== 'arch'));
  globalThis.GEX_PAGE_SEARCH = `?${new URLSearchParams(sw)}`;
  globalThis.GEX_ASSET_BASE = t.baseUrl;
  setGlobals((n) => sw[n] ?? null, { arch: dbg.arch ?? 'aarch64' });
  const tree = untar(await unzstd(await bytesOf(t.work('bb.tar.zst'))));
  const lazyUrl = new URL(t.work('lazy-program.bin'));
  const lazyAt = lazyUrl.protocol === 'file:' ? decodeURIComponent(lazyUrl.pathname) : lazyUrl.pathname;

  let out = '';
  let keys = null;
  const term = {
    rows: 24, cols: 80,
    write: (s) => { out += typeof s === 'string' ? s : new TextDecoder().decode(s); },
    writeln: (s) => { out += `${s}\n`; },
    onData: (fn) => { keys = fn; return { dispose() { keys = null; } }; },
    onResize: () => ({ dispose() {} }),
    focus() {},
  };
  const script = [
    'rt-probe tty',
    'echo "ppid $PPID"; cat /proc/1/comm',
    'ls -i /bin/sh /bin/ls | sort -k2 | awk \'{print $1}\' | uniq | wc -l | sed "s/^/inodes /"',
    'ls -l /bin/rt-probe | cut -c1-10 | sed "s/^/mode /"',
    'rt-probe xattr /etc/xattr user.rt',
    'date -r /etc/hello +%s | sed "s/^/mtime /"',
    'cat /etc/hello',
    'wc -c < /lazy.bin | sed "s/^/lazy /"',
    'readlink /etc/link',
    'readlink /lbin | sed "s/^/lbin /"',
    'read x; echo "got $x"',
    'echo last line; exit 7',
  ].join('\n');
  const t0 = performance.now();
  const run = runProgram(term, {
    filesystem: 'memory',
    elf: tree.files[tree.hard['/bin/sh'] ?? '/bin/sh'],
    // The program by a directory link's name (/bin/bash, /bin -> usr/bin).
    args: ['/lbin/sh', '-c', script],
    files: { ...tree.files, '/etc/hello': 'hello\n' },
    dirs: [...(tree.dirs ?? []), '/empty'],
    modes: tree.modes,
    hard: tree.hard,
    symlinks: { ...(tree.links ?? {}), '/etc/link': '/etc/hello', '/lbin': 'bin' },
    xattrs: tree.xattrs,
    mtimes: { '/etc/hello': 1000000000 },
    lazy: [['/lazy.bin', 3 << 20, lazyAt]],
    env: ['PATH=/bin:/usr/bin', 'HOME=/root'],
    cwd: '/',
  });
  run.catch((e) => { out += `\n[runProgram threw: ${e?.stack ?? e}]`; });
  await timeout(until(() => /\/dev\/pts\//.test(out), 'the terminal', 60000), 70000, 'start')
    .catch((e) => { throw new Error(`${e.message}: ${out.slice(-600)}`); });
  say(`the program runs in ${(performance.now() - t0).toFixed(0)} ms`);
  keys?.('typed\r');
  const r = await timeout(run, 30000, 'the end');
  const lines = out.replace(/\r/g, '');
  say(lines.split('\n').map((l) => `  | ${l}`).join('\n'));
  eq('the program\'s status', r.status, 7);
  check('the program on a terminal', /^\/dev\/pts\/\d+$/m.test(lines), lines.slice(0, 200));
  // (The terminal echoes the typed keys wherever they fall.)
  check('a child of the kernel\'s pid 1', /^ppid 1$/m.test(lines) && /^init$/m.test(lines));
  check('hard links are one inode', /^inodes 1$/m.test(lines));
  check('a file\'s mode', /^mode -rwxr-xr-x$/m.test(lines));
  check('an extended attribute', /^kept$/m.test(lines));
  check('a file time', /^mtime 1000000000$/m.test(lines));
  check('a file of the caller\'s over the bundle\'s', /^hello$/m.test(lines));
  check('a lazy file', new RegExp(`^lazy ${3 << 20}$`, 'm').test(lines));
  check('a symbolic link', /^\/etc\/hello$/m.test(lines));
  check('the program by a directory link\'s name, the link kept', /^lbin bin$/m.test(lines));
  check('keys from the terminal', /^got typed$/m.test(lines));
  check('the last line before the system stops', /^last line$/m.test(lines));
  eq('the system stopped', r.system?.state.phase, 'stopped');
}
