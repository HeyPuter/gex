// Threads and processes under gexc and QEMU (lib/qemu.mjs): pthreads whose
// results are order-independent (the arithmetic compared, not the
// scheduler), pause, sigsuspend and the no-descriptor polls ended by another
// thread's signal, exit while threads are mid-work (two parked on a futex),
// fork, a child still running while its parent works, a signal delivered while
// parked in a syscall, C++ exceptions, and a guest writing its own machine
// code (runtime translation on: the code does not exist until the guest writes
// it). Threads again with --no-jit. Then three without QEMU as the oracle: exec
// (argv, envp, close-on-exec, a failed exec returning), pid 1 (sessions, an
// orphan adopted, a process group stopped with one signal: QEMU cannot agree
// on which process is pid 1) and threads and pipes in a forked then exec'd
// child (what a package manager does to unpack an archive).
import { qemu } from '../lib/qemu.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 900 };

export default async function (t) {
  const q = qemu(t);
  const np = ['-static', '-no-pie', '-O2'];
  const threads = await q.build('threads', 'threads.c', [...np, '-lpthread']);
  const exitrace = await q.build('exitrace', 'exitrace.c', [...np, '-lpthread']);
  const sigpause = await q.build('sig-pause', 'sig-pause.c', [...np, '-lpthread']);
  const fork = await q.build('fork', 'fork.c', np);
  const background = await q.build('background', 'background.c', np);
  const blocked = await q.build('sigblocked', 'signal-while-blocked.c', np);
  const execp = await q.build('exec', 'exec.c', np);
  const init = await q.build('init', 'init.c', np);
  const forkthread = await q.build('forkthread', 'forkthread.c', [...np, '-pthread']);
  const selfmod = await q.build('selfmod', 'selfmod.c', ['-static', '-O2']);
  const cpp = await q.buildCxx('cpp', 'cpp.cc', np);

  await q.compare('threads', threads);
  await q.compare('sig-pause', sigpause);
  await q.compare('thread-exit-race', exitrace);
  await q.compare('fork', fork);
  // The browser host once froze the parent until the child exited, so a
  // daemon started with `&` stopped the shell for good.
  await q.compare('background', background);
  // The browser once dropped such a delivery, and every fork from a shell
  // (SIGCHLD the first handler most programs install) ended the run.
  await q.compare('signal-while-blocked', blocked);
  await q.compare('cpp-static', cpp);
  await q.compare('nojit-threads', threads, [], { gexc: ['--no-jit'] });
  await q.compare('selfmod', selfmod);

  await q.expect('exec', execp, [], `argc=4 [child] [two] [three]
env GEXA=one GEXB=two
fd7 closed  fd8 open
inherited fd reads 16 bytes: through the pipe
child status 42
failed exec status 99
done
`);
  await q.expect('init', init, [], `init: pid=1 ppid=0 pgid=1 sid=1
init: setsid again -> EPERM
service: pid 2
service: pid 2 exited 0
adopted: pid 3 exited 1
group: led by 4 with 5
group: both joined -> yes
group: kill(-4, SIGTERM) -> 0
group: pid 4 exited 42
group: pid 5 exited 42
init: wait with no children -> ECHILD
init: done
`);
  await q.expect('fork-exec-threads', forkthread, [], `worker: 4 threads joined, mix=877
main: worker exited 0
sink: 140434 bytes summing to 983040
main: sink exited 0
`);
}
