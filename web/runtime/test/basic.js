// System, spawn (pipes and a terminal), run, kill, and the Filesystem.
import { System } from '../src/index.js';
import { Filesystem } from '../src/fs.js';
import { main, say, check, eq, timeout } from './lib.js';
import { testDebug } from './lib.js';

main(async () => {
  const sys = new System({ baseUrl: '/', debug: testDebug() });
  const fs = new Filesystem(sys);
  fs.addTar('/__rt/bb.tar.zst');
  fs.addFiles({ '/etc/hello': 'hello from addFiles\n' });
  check('a second System throws', (() => { try { new System(); return false; } catch (e) { return /one system/.test(e.message); } })());
  check('a second Filesystem throws', (() => { try { new Filesystem(sys); return false; } catch { return true; } })());
  const t0 = performance.now();
  await timeout(sys.boot(), 60000, 'boot');
  say(`booted in ${(performance.now() - t0).toFixed(0)} ms`);
  eq('state', sys.state.phase, 'running');

  const env = { PATH: '/bin:/usr/bin', HOME: '/root' };
  // Pid 1 is the kernel's: a program's parent, an orphan's, and named init.
  eq('a program is pid 1\'s child', (await sys.run(['sh', '-c', 'echo $PPID'], { env })).stdout, '1\n');
  eq('/proc/1', (await sys.run(['cat', '/proc/1/comm'], { env })).stdout, 'init\n');
  eq('a signal to pid 1 is ignored', (await sys.run(['sh', '-c', 'rt-probe kill 1 15 && rt-probe kill 1 9 && rt-probe kill 1 0'], { env })).stdout, '0\n0\n0\n');
  const orphan = await sys.run(['sh', '-c', 'sh -c \'sleep 0.3; exec rt-probe ppid > /tmp/orphan-ppid\' > /dev/null 2>&1 & echo $!'], { env });
  const opid = orphan.stdout.trim();
  const ppidSeen = async () => fs.readFile('/tmp/orphan-ppid', { encoding: 'utf-8' }).catch(() => '');
  for (let i = 0; i < 100 && !(await ppidSeen()); i++) await new Promise((r) => setTimeout(r, 50));
  eq('an orphan is given to pid 1', await ppidSeen(), '1\n');
  let reaped = false;
  for (let i = 0; i < 100 && !reaped; i++) {
    reaped = !(await fs.stat(`/proc/${opid}`).then(() => true, () => false));
    if (!reaped) await new Promise((r) => setTimeout(r, 50));
  }
  check('an orphan that ends is collected', reaped, `/proc/${opid} still there`);
  eq('an empty argument is an argument', (await sys.run(['sh', '-c', 'echo $#', 'sh', '', 'b'], { env })).stdout, '2\n');
  let t = performance.now();
  const r = await timeout(sys.run(['sh', '-c', 'echo out; echo err >&2; cat /etc/hello; exit 3'], { env }), 30000, 'run');
  say(`run in ${(performance.now() - t).toFixed(0)} ms`);
  eq('run stdout', r.stdout, 'out\nhello from addFiles\n');
  eq('run stderr', r.stderr, 'err\n');
  eq('run code', r.code, 3);

  const c = await timeout(sys.run(['cat'], { env, input: 'piped through\n' }), 30000, 'cat');
  eq('stdin piped', c.stdout, 'piped through\n');

  const big = 'x'.repeat(300000) + '\n';
  const w = await timeout(sys.run(['wc', '-c'], { env, input: big }), 30000, 'wc');
  eq('300 kB through a pipe', w.stdout.trim(), String(big.length));

  const cwd = await sys.run(['pwd'], { env, cwd: '/tmp' });
  eq('cwd', cwd.stdout, '/tmp\n');
  const id = await sys.run(['rt-probe', 'ids'], { env, uid: 1000, gid: 100 });
  eq('uid', id.stdout, '1000 100\n');
  // ... and only that program: the system's own uid is still root's.
  eq('uid after', (await sys.run(['rt-probe', 'ids'], { env })).stdout, '0 0\n');
  const e2 = await sys.run(['printenv', 'FOO'], { env: { ...env, FOO: 'bar' } });
  eq('env', e2.stdout, 'bar\n');

  const missing = sys.spawn(['/no/such/program'], { env });
  const err = await missing.started.then(() => null, (e) => e);
  check('a missing program rejects', err && /no such program/.test(err.message), String(err));

  // A terminal.
  const p = sys.spawn(['sh'], { pty: { cols: 100, rows: 30, encoding: 'utf-8' }, env: { ...env, PS1: '$ ', TERM: 'xterm' } });
  let screen = '';
  p.pty.addEventListener('data', (e) => { screen += e.data; });
  await timeout(p.started, 30000, 'pty start');
  check('pty pid', p.pid > 1, String(p.pid));
  p.pty.write('rt-probe size; rt-probe tty; echo forty""-two\n');
  const until = async (re, what) => {
    for (let i = 0; i < 300 && !re.test(screen); i++) await new Promise((r) => setTimeout(r, 50));
    return check(what, re.test(screen), JSON.stringify(screen.slice(-300)));
  };
  await until(/30 100/, 'pty size');
  await until(/\/dev\/pts\/\d+/, 'pty is a tty');
  await until(/forty-two/, 'pty runs commands');
  p.pty.resize(120, 40);
  p.pty.write('rt-probe size\n');
  await until(/40 120/, 'pty resize');
  p.pty.write('sleep 100\n');
  await new Promise((r) => setTimeout(r, 500));
  p.pty.write('\x03');
  p.pty.write('echo after-ctrl-c\n');
  await until(/after-ctrl-c\r?\n/, '^C interrupts the foreground job');
  p.pty.write('exit 5\n');
  const st = await timeout(p.exited, 30000, 'pty exit');
  eq('pty exit', st, { code: 5, signal: null });

  // kill
  const s = sys.spawn(['sleep', '100'], { env });
  await s.started;
  s.kill();
  eq('kill', await timeout(s.exited, 30000, 'kill'), { code: 143, signal: 'SIGTERM' });

  // detached
  const d = sys.spawn(['sh', '-c', 'echo detached > /tmp/detached'], { env, detached: true });
  eq('detached exit', await timeout(d.exited, 30000, 'detached'), { code: 0, signal: null });
  eq('detached wrote', await fs.readFile('/tmp/detached', { encoding: 'utf-8' }), 'detached\n');

  // The filesystem from the page.
  await fs.writeFile('/root/note.txt', 'from the page\n');
  eq('guest reads the page\'s file', (await sys.run(['cat', '/root/note.txt'], { env })).stdout, 'from the page\n');
  await sys.run(['sh', '-c', 'mkdir -p /root/d/e && echo guest > /root/d/e/f && ln -s e/f /root/d/link'], { env });
  eq('page reads the guest\'s file', await fs.readFile('/root/d/e/f', { encoding: 'utf-8' }), 'guest\n');
  const stt = await fs.stat('/root/d/e/f');
  eq('stat', [stt.type, stt.size, stt.mode], ['file', 6, 0o644]);
  eq('lstat of a symlink', (await fs.stat('/root/d/link', { follow: false })).type, 'symlink');
  eq('stat follows', (await fs.stat('/root/d/link')).type, 'file');
  eq('readlink', await fs.readlink('/root/d/link'), 'e/f');
  eq('readdir', (await fs.readdir('/root/d')).map((x) => `${x.name}:${x.type}`).sort(), ['e:directory', 'link:symlink']);
  await fs.mkdir('/root/x/y/z', { recursive: true });
  await fs.rename('/root/note.txt', '/root/x/y/z/note.txt');
  eq('rename', (await sys.run(['cat', '/root/x/y/z/note.txt'], { env })).stdout, 'from the page\n');
  await fs.symlink('/root/x', '/root/xl');
  eq('symlink', (await sys.run(['readlink', '/root/xl'], { env })).stdout, '/root/x\n');
  await fs.chmod('/root/x/y/z/note.txt', 0o600);
  eq('chmod', (await fs.stat('/root/x/y/z/note.txt')).mode, 0o600);
  await fs.rm('/root/x', { recursive: true });
  eq('rm -r', (await sys.run(['ls', '/root'], { env })).stdout.split('\n').filter(Boolean).sort(), ['d', 'xl']);
  const nf = await fs.readFile('/nope').then(() => null, (e) => e.code);
  eq('ENOENT', nf, 'ENOENT');

  eq('a tar\'s xattrs', (await sys.run(['rt-probe', 'xattr', '/etc/xattr', 'user.rt'], { env })).stdout, 'kept\n');
  await fs.addTar('/__rt/bb.tar.zst', { at: '/opt/x' });
  eq('a late tar\'s xattrs', (await sys.run(['rt-probe', 'xattr', '/opt/x/etc/xattr', 'user.rt'], { env })).stdout, 'kept\n');

  // Watching a directory: the guest's changes and the page's.
  await fs.mkdir('/root/w');
  const seen = [];
  const stop = fs.watch('/root/w', (e) => seen.push(`${e.type} ${e.path}`));
  await new Promise((r) => setTimeout(r, 100));
  await sys.run(['sh', '-c', 'echo a > /root/w/f; mv /root/w/f /root/w/g; rm /root/w/g'], { env });
  await fs.writeFile('/root/w/page', 'p');
  for (let i = 0; i < 40 && seen.length < 7; i++) await new Promise((r) => setTimeout(r, 50));
  eq('watch', seen, ['create /root/w/f', 'change /root/w/f', 'move-out /root/w/f', 'move-in /root/w/g', 'delete /root/w/g', 'create /root/w/page', 'change /root/w/page']);
  stop();
  await fs.writeFile('/root/w/after', 'p');
  await new Promise((r) => setTimeout(r, 200));
  eq('watch stops', seen.length, 7);

  // A tar after boot.
  await fs.addFiles({ '/opt/late/a.txt': 'late file\n' });
  eq('addFiles after boot', (await sys.run(['cat', '/opt/late/a.txt'], { env })).stdout, 'late file\n');
  await fs.addTar('/__rt/bb.tar.zst', { at: '/opt/bb' });
  eq('addTar after boot', (await sys.run(['/opt/bb/bin/echo', 'from the late tar'], { env })).stdout, 'from the late tar\n');

  say(JSON.stringify(sys.describe()));
  await sys.shutdown();
  eq('shutdown', sys.state.phase, 'stopped');
});
