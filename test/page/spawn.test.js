// glibc's posix_spawn, from a parent of four threads (fixtures/programs/
// spawn-threads.c, dynamically linked). glibc clones with
// clone3(CLONE_VM | CLONE_VFORK) onto a stack of its own; the page runs the
// child in the calling thread's Worker until its exec (shims/browser/src/
// vfork.rs). Every child (24 spawns and a missing program) must have run in
// place rather than as a copied fork, and their programs start on runtimes
// their Workers made in the host's pool (`preattach`): at least one does,
// and none fails to be made or is dropped as not fitting -- under the mem64
// seed every one failed ("cannot import i32 memory as i64") and each spawn
// quietly paid for a new one.
import { readFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { guest, pageLegs, tail, count } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], timeout: 300 };

const WANT = '24 of 24 children right\nparent: fd5 open, usr2 handled, usr1 open\nmissing program: ENOENT\n3 of 3 other threads ran';

export default async function (t) {
  const g = guest(t);
  const [leg] = pageLegs(g);
  const prog = g.cc('spawn-threads.c', { glibc: true });
  const trace = join(t.workDir, 'guest', `spawn-${leg.label}.trace`);
  rmSync(trace, { force: true });
  const r = await g.run(leg.seed, prog, [], { files: { '/bin/spawn-threads': prog, ...g.glibc() }, argv0: '/bin/spawn-threads', env: { ...leg.env, GEX_VFORK_TRACE: trace }, timeout: 180000 });
  let tr = '';
  try { tr = readFileSync(trace, 'utf8'); } catch {}
  t.eq('spawn-threads', [r.code, r.guest], [0, WANT], tail(r.stderr, 3));
  t.eq('children run in place', count(tr, /^\[vfork\] pid \d+: (exec|exit)/), 25);
  t.check(`programs on pooled runtimes (${count(tr, /^\[pool\] pid \d+: pooled runtime/)})`, count(tr, /^\[pool\] pid \d+: pooled runtime/) >= 1);
  const unfit = r.stderr.split('\n').filter((l) => /could not prepare a runtime|pooled runtime does not fit/.test(l));
  t.check('no pooled runtime failed or unfit', !unfit.length, unfit.slice(0, 3).join(' / '));
}
