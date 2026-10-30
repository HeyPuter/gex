// runProgram (web/src/runtime.js) as a preset over the runtime API
// (run-program.js): an untarred bundle laid into the root with its hard
// links, symbolic links, modes, extended attributes, file times and a lazy
// file; the program pid 1 on a terminal bridged to an xterm-shaped object;
// keys typed into it; its exit status; and its last output before the
// system stops.
import { runProgram } from '../../web/src/runtime.js';
import { setGlobals } from '../../web/src/page-globals.js';
import { untar } from '../../web/src/tar.js';
import { unzstd } from '../../web/src/zfetch.js';
import { bytesOf } from '../../web/runtime/src/internal.js';

export const meta = { hosts: ['browser', 'node'], images: ['busybox'], timeout: 180 };

export default async function (t) {
  const { say, check, eq, until, timeout } = t;
  const dbg = t.debug();
  const sw = Object.fromEntries(Object.entries(dbg).filter(([k]) => k !== 'arch'));
  globalThis.GEX_PAGE_SEARCH = `?${new URLSearchParams(sw)}`;
  globalThis.GEX_ASSET_BASE = t.baseUrl;
  setGlobals((n) => sw[n] ?? null, { arch: dbg.arch ?? 'aarch64' });
  const tree = untar(await unzstd(await bytesOf(t.work('bb.tar.zst'))));
  const lazyUrl = new URL(t.work('lazy.bin'));
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
    'ls -i /bin/sh /bin/ls | sort -k2 | awk \'{print $1}\' | uniq | wc -l | sed "s/^/inodes /"',
    'ls -l /bin/rt-probe | cut -c1-10 | sed "s/^/mode /"',
    'rt-probe xattr /etc/xattr user.rt',
    'date -r /etc/hello +%s | sed "s/^/mtime /"',
    'cat /etc/hello',
    'wc -c < /lazy.bin | sed "s/^/lazy /"',
    'readlink /etc/link',
    'read x; echo "got $x"',
    'echo last line; exit 7',
  ].join('\n');
  const t0 = performance.now();
  const run = runProgram(term, {
    elf: tree.files[tree.hard['/bin/sh'] ?? '/bin/sh'],
    args: ['/bin/sh', '-c', script],
    files: { ...tree.files, '/etc/hello': 'hello\n' },
    dirs: [...(tree.dirs ?? []), '/empty'],
    modes: tree.modes,
    hard: tree.hard,
    symlinks: { ...(tree.links ?? {}), '/etc/link': '/etc/hello' },
    xattrs: tree.xattrs,
    mtimes: { '/etc/hello': 1000000000 },
    lazy: [['/lazy.bin', 3 << 20, lazyAt]],
    env: ['PATH=/bin:/usr/bin', 'HOME=/root'],
    cwd: '/',
  });
  run.catch((e) => { out += `\n[runProgram threw: ${e?.stack ?? e}]`; });
  await timeout(until(() => /\/dev\/pts\//.test(out), 'the terminal', 60000), 70000, 'start')
    .catch((e) => { throw new Error(`${e.message}: ${out.slice(-600)}`); });
  say(`pid 1 runs in ${(performance.now() - t0).toFixed(0)} ms`);
  keys?.('typed\r');
  const r = await timeout(run, 30000, 'the end');
  const lines = out.replace(/\r/g, '');
  say(lines.split('\n').map((l) => `  | ${l}`).join('\n'));
  eq('pid 1\'s status', r.status, 7);
  check('pid 1 on a terminal', /^\/dev\/pts\/\d+$/m.test(lines), lines.slice(0, 200));
  check('hard links are one inode', /^inodes 1$/m.test(lines));
  check('a file\'s mode', /^mode -rwxr-xr-x$/m.test(lines));
  check('an extended attribute', /^kept$/m.test(lines));
  check('a file time', /^mtime 1000000000$/m.test(lines));
  check('a file of the caller\'s over the bundle\'s', /^hello$/m.test(lines));
  check('a lazy file', new RegExp(`^lazy ${3 << 20}$`, 'm').test(lines));
  check('a symbolic link', /^\/etc\/hello$/m.test(lines));
  check('keys from the terminal', /^got typed$/m.test(lines));
  check('the last line before the system stops', /^last line$/m.test(lines));
  eq('the system stopped', r.system?.state.phase, 'stopped');
}
