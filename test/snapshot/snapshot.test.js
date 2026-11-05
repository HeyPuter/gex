// Session snapshot and restore (test/snapshot/snapshot.mjs):
//   - snaptest.c: three threads in a futex wait, epoll_wait and nanosleep,
//     snapshotted 2 s into its 4 s sleep and restored 3 s later; its whole
//     run must be the host's (the same source, the host's cc); restored again
//     and asked for a snapshot, it refuses (status 3) and says why;
//   - snapsrv.c: wineserver in the session (GEX_TEST_WINE64_SERVER, wine/dist.sh's
//     WINE64_SERVER: its objects, linked here with the flavour; Wine's NLS
//     from GEX_TEST_WINE64_DIST);
//   - snapwait.c and snapnest.c, Windows x64 programs on 64-bit Wine, with
//     GEX_TEST_SNAP_WINE64_DIST and _PREFIX (an install built against the
//     wasm64 snapshot sysroot, build-sysroot64.sh with GEX_SNAPSHOT=1, and
//     its prefix; _NATIVE: its native PE core): threads in GetMessage and
//     WaitForSingleObject, the main thread in a 4 s Sleep, without the
//     server's door and with it, and again with every call of Wine's by the
//     fault hook's route (GEX_TEST_UD2_ENTRY); snapnest a thread waiting in a
//     GetMessage inside its window's WM_CREATE. Said and left out without them.
import { execFileSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdirSync, readdirSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { tail, ROOT } from '../lib/guest.mjs';
import { snap, FIX } from './snapshot.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1800 };

export default async function (t) {
  const s = snap(t);
  const { exe, o } = s.snaptest();
  const host = join(s.dir, 'snaptest.native');
  execFileSync(s.g.tool('cc'), ['-O1', '-o', host, join(FIX, 'snaptest.c'), '-lpthread']);
  const native = execFileSync(host, { encoding: 'utf8' });
  const c = await s.case('snaptest', exe, [], o, { delay: 2000, gap: 3 });
  if (c) {
    t.check('snaptest: the whole run is the host\'s', s.sorted(c.whole.stdout) === s.sorted(native), tail(c.whole.stdout));
    // a restored session takes no snapshot
    const again = join(s.dir, 'snaptest.again');
    const r = await s.run(exe, [], o, { GEX_SNAP_IN: c.snap, GEX_SNAP_OUT: again, GEX_SNAP_AT: 'cond: woke', GEX_SNAP_DELAY_MS: '0' });
    t.check('snaptest: a restored session refuses a snapshot', r.code === 3 && r.stderr.includes('a restored session takes no snapshot') && !existsSync(again), `exit ${r.code}: ${tail(r.stderr)}`);
  }

  const server = process.env.GEX_TEST_WINE64_SERVER;
  const dist = process.env.GEX_TEST_WINE64_DIST;
  if (!server || !dist) return t.say('snapsrv, snapwait: left out (GEX_TEST_WINE64_SERVER and GEX_TEST_WINE64_DIST: wine/dist.sh\'s WINE64_SERVER and WINE64_DIST)');
  const srv = join(s.dir, 'srv');
  mkdirSync(srv);
  const objs = readdirSync(join(server, 'obj')).filter((f) => f.endsWith('.o')).map((f) => join(server, 'obj', f));
  s.wcc(['-O1', '-o', 'wineserver', ...objs], srv);
  const ss = join(s.dir, 'snapsrv');
  s.wcc(['-O2', '-o', ss, join(FIX, 'snapsrv.c')]);
  await s.case('snapsrv', ss, [], {
    files: { '/t/snapsrv': ss, '/t/wineserver': join(srv, 'wineserver'), '/usr/share/wine/nls/l_intl.nls': join(dist, 'share/wine/nls/l_intl.nls') },
    argv0: './snapsrv', flags: ['--cwd', '/t', '--env', 'WINESERVER=/t/wineserver'],
  });

  const sd = process.env.GEX_TEST_SNAP_WINE64_DIST;
  const sp = process.env.GEX_TEST_SNAP_WINE64_PREFIX;
  if (!sd || !sp) return t.say('snapwait, snapnest: left out (GEX_TEST_SNAP_WINE64_DIST and _PREFIX: an install built against the wasm64 snapshot sysroot)');
  const gcc = s.g.tool('x86_64-w64-mingw32-gcc');
  for (const n of ['snapwait', 'snapnest']) execFileSync(gcc, ['-O2', '-o', join(s.dir, `${n}.exe`), join(FIX, `${n}.c`)]);
  // the install with the flavour's server (a page's mount does not follow a
  // link out of it: hard links, or a copy on another file system)
  const d = join(s.dir, 'dist64');
  execFileSync('cp', ['-rl', sd, d]);
  const unix = join(d, 'lib/wine/x86_64-unix');
  for (const f of ['wineserver', 'wineserver.wasm']) {
    rmSync(join(unix, f), { force: true });
    if (existsSync(join(srv, f))) copyFileSync(join(srv, f), join(unix, f));
  }
  const pfx = join(s.dir, 'pfx64');
  execFileSync(join(ROOT, 'wine/page-prefix.sh'), [sp, d, pfx], { stdio: ['ignore', 'ignore', 'pipe'] });
  for (const n of ['snapwait', 'snapnest']) copyFileSync(join(s.dir, `${n}.exe`), join(pfx, 'drive_c', `${n}.exe`));
  const wine = (dd, door, exe, node = {}) => ({
    argv0: '/wine64/lib/wine/x86_64-unix/wine', env: node,
    flags: ['--dir', `/wine64=${dd}`, '--dir', `/pfx=${pfx}`, '--env', 'WINEPREFIX=/pfx', '--env', 'WINEDEBUG=err+all,fixme-all', '--env', `WINEGEX_DOOR=${door}`, '--cwd', '/pfx/drive_c'],
    exe,
  });
  const legs = [['snapwait', d, 'snapwait', {}], ['snapwait-ud2', d, 'snapwait', { GEX_TEST_UD2_ENTRY: '1' }], ['snapnest', d, 'snapnest', {}]];
  const nat = process.env.GEX_TEST_SNAP_WINE64_NATIVE;
  if (nat) {
    // the same install, the core's files in place of its DLLs
    const dn = join(s.dir, 'dist64n');
    execFileSync('cp', ['-rl', d, dn]);
    for (const f of readdirSync(nat)) {
      rmSync(join(dn, 'lib/wine/x86_64-windows', f), { force: true });
      copyFileSync(join(nat, f), join(dn, 'lib/wine/x86_64-windows', f));
    }
    legs.push(['snapwait-native', dn, 'snapwait', {}]);
  } else t.say('snapwait-native: left out (GEX_TEST_SNAP_WINE64_NATIVE)');
  for (const [name, dd, exe, node] of legs)
    for (const door of [0, 1]) {
      const w = wine(dd, door, exe, node);
      await s.case(door ? `${name}-door` : name, join(dd, 'lib/wine/x86_64-unix/wine'), [`C:\\${w.exe}.exe`], w, { delay: 2000, gap: 3, timeout: 300000 });
    }
}
