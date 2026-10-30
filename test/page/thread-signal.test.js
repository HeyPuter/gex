// A signal reaches a thread spinning in its own code. Such a thread makes no
// syscalls, so only a back-edge safepoint can deliver to it. A `pthread_kill`
// from another thread raises the signal in that process's kernel for that
// thread, and the safepoint once asked about whichever thread last entered
// the kernel instead: the handler never ran and the join never returned.
// Five rounds of that, then an `alarm` that must end a spin on the main
// thread (the timer path the same check serves). The safepoint takes the
// kernel's lock only on a hint, and this is what the hints must not miss.
import { guest, tail } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], timeout: 180 };

export default async function (t) {
  const g = guest(t);
  g.pub('gex_browser{infix}.shared.wasm');
  g.pub('gex_kernel{infix}.wasm');
  const r = await g.run(g.seed(), g.cc('thread-signal.c', { flags: ['-lpthread'] }), [], { env: { GEX_SHARED: '1' } });
  t.eq('a pthread_kill reaches a spinning thread, and an alarm a spinning main thread', r.guest,
    'spinning threads took SIGUSR1 promptly: yes\nalarm ended a spin: yes', tail(r.stderr, 3));
}
