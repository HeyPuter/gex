// Output ordering across processes under the page runtime's ProcessHost. A
// child that writes, flushes and exits, and a parent that waits for it and
// then writes: the child's line must come first. Each process is its own
// Worker and its output reaches the host as that Worker's messages, which
// nothing orders against another Worker's, so the host has to be told a
// process's output is delivered before its exit can be seen (`drain_output`,
// hosts/browser). Stressed: several copies at once, each forking many times,
// which is when the misorder showed (7 of 16 runs).
import { guest, tail } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], timeout: 600 };

const COPIES = 16;
const FORKS = 20;

export default async function (t) {
  const g = guest(t);
  g.pub('gex_browser{infix}.shared.wasm');
  g.pub('gex_kernel{infix}.wasm');
  const prog = g.cc('output-order.c');
  const seed = g.seed();
  const want = Array.from({ length: FORKS }, (_, i) => `child ${i}\nparent ${i}`).join('\n');
  const runs = await Promise.all(Array.from({ length: COPIES }, () => g.run(seed, prog, [FORKS], { env: { GEX_SHARED: '1' }, timeout: 300000 })));
  const bad = runs.filter((r) => r.guest !== want);
  t.check(`${COPIES} runs x ${FORKS} forks, every child's output before its parent's`, !bad.length,
    bad.length ? `${bad.length} out of order; first: ${tail(bad[0].guest, 6)} (${tail(bad[0].stderr, 3)})` : '');
}
