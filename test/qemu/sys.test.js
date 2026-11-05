// System calls under gexc and QEMU (lib/qemu.mjs): the VFS, signals (every
// disposition, a caught SIGILL, fatal by default and by abort, pending sets,
// kill, alarm, SIGCHLD), priorities, renameat2, sendfile, openat2,
// utimensat, link counts, interval timers, an event loop, unix sockets with
// descriptor passing, partial vectored I/O and what brk moves over reading as
// zero (the second half with --no-jit). Then two without QEMU as the oracle:
// shebang scripts, flock and epoll (qemu-aarch64 can re-exec only the program
// it was given, so a script's execve is ENOEXEC there; the expectation is
// Linux's), and how much of struct termios a TCGETS writes, both sides under
// script(1) for a terminal.
import { exec } from '../lib/native.mjs';
import { qemu } from '../lib/qemu.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 900 };

const HOSTFACILITIES = `interp arg1 [--interp]
interp got: [shebang1] [extra]
script 1 exited 4
interp arg1 [--interp with two words]
interp got: [shebang2] [extra]
script 2 exited 4
exec 3 failed: ENOEXEC
script 3 exited 9
exclusive: 0
second on the same file: -1 EWOULDBLOCK
a different file: 0
through a dup: 0
unlock: 0
after unlock: 0
shared then shared: 0 0
add: 0
add twice: -1 EEXIST
nothing ready: 0
after a write: 1 events=EPOLLIN data=feedface
mod: 0
after mod: 1 data=1234
del: 0
del twice: -1 ENOENT
after del: 0
`;

export default async function (t) {
  const q = qemu(t);
  const p = {};
  for (const name of ['vfs', 'signals']) p[name] = await q.build(name, `${name}.c`, ['-static', '-no-pie', '-O2']);
  for (const name of ['sigpending', 'priority', 'renameat2', 'sendfile', 'openat2', 'utimensat', 'nlink', 'itimer', 'eventloop', 'unixsock', 'hostfacilities',
    'termios-size', 'unixsock-visible', 'iov-partial', 'signal-kill', 'signal-alarm', 'signal-sigchld']) p[name] = await q.build(name, `${name}.c`, ['-static', '-O2']);
  p['brk-zero'] = await q.build('brk-zero', 'brk-zero.c', ['-static', '-no-pie', '-O2']);

  await q.compare('vfs', p.vfs);
  await q.compare('signal-all', p.signals, ['all']);
  await q.compare('signal-catch-illegal', p.signals, ['catch-illegal']);
  for (const name of ['sigpending', 'priority', 'renameat2', 'sendfile', 'openat2', 'utimensat', 'nlink']) await q.compare(name, p[name]);
  for (const mode of ['fatal-default', 'fatal-abort']) await q.compare(`signal-${mode}`, p.signals, [mode]);
  await q.compare('itimer', p.itimer);
  await q.compare('eventloop', p.eventloop);
  await q.compare('unixsock', p.unixsock);
  const nojit = { gexc: ['--no-jit'] };
  for (const name of ['unixsock-visible', 'iov-partial', 'brk-zero', 'signal-kill', 'signal-alarm', 'signal-sigchld']) await q.compare(name, p[name], [], nojit);

  await q.expect('hostfacilities', p.hostfacilities, [], HOSTFACILITIES);

  // GEX_LOG=off: gex's own process-start line goes to stderr, which the
  // terminal merges into what is compared. A run with no terminal has nothing
  // to ask and says so, and a vacuous pass is worse than none: `clobbered 0`
  // must be there.
  const pty = async (cmd) => (await exec(['script', '-qec', cmd, '/dev/null'], { cwd: q.dir, env: q.n.env })).stdout.replace(/\r/g, '');
  const g = await pty(`env GEX_LOG=off ${q.n.gexcPath} run ${p['termios-size']}`);
  const want = await pty(`${q.qemuPath} ${p['termios-size']}`);
  t.check('termios-size', g === want && g.includes('clobbered 0'), `gex ${JSON.stringify(g)}, qemu ${JSON.stringify(want)}`);
}
