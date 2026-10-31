// A process's Worker given back to the pool (host.mjs `#giveBack`,
// worker.mjs `reset`): fixtures/programs/reuse-state.c starts processes that
// leave state behind (an interval timer, signal handlers, a umask, a cwd, a
// thread parked reading a pipe) and checks that the processes after them
// see none of it, started by posix_spawn, vfork + exec, fork + exec and an
// exec from a second thread. Some of them must have started on given-back
// Workers -- none in the single-thread build, whose processes are fibers of
// one realm.
import { readFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { guest, pageLegs, tail, count } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], timeout: 900 };

const ROUNDS = 6;

export default async function (t) {
  const g = guest(t);
  const legs = pageLegs(g, { sp: true });
  const prog = g.cc('reuse-state.c', { glibc: true, flags: ['-pthread'] });
  const files = { '/bin/reuse-state': prog, ...g.glibc() };
  for (const leg of legs) {
    const trace = join(t.workDir, 'guest', `reuse-${leg.label}.trace`);
    rmSync(trace, { force: true });
    const r = await g.run(leg.seed, prog, [ROUNDS], { files, argv0: '/bin/reuse-state', env: { ...leg.env, GEX_VFORK_TRACE: trace }, single: leg.single, timeout: 300000 });
    let back = 0;
    try { back = count(readFileSync(trace, 'utf8'), /^\[pool\] pid \d+: given-back worker/); } catch {}
    t.eq(`reuse-state (${leg.label})`, [r.code, r.guest], [0, `reuse-state ok ${ROUNDS}`], tail(r.stderr, 3));
    t.check(`${back} processes on given-back workers (${leg.label})`, leg.single ? back === 0 : back >= 1);
  }
}
