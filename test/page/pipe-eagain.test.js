// Blocking pipe writes never see EAGAIN. `yes | head -c 20M | wc -c` in a
// guest, eight times in one process tree: a producer writing as fast as it
// can, a "head" copying exactly N bytes (alternating write and writev), and
// a counter; each round once with one head and once with two writing into
// the same pipe, where another Worker's writer can fill the pipe between the
// runtime's room check and the write. A head told EAGAIN on its blocking
// descriptor exits 3 (7 for a read), and the count comes up short. Seen once
// in the page: 65536 instead of 20000000.
import { guest, tail } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], timeout: 1260 };

const ROUNDS = 8;
const MIB = 20;

export default async function (t) {
  const g = guest(t);
  g.pub('gex_browser{infix}.shared.wasm');
  g.pub('gex_kernel{infix}.wasm');
  const r = await g.run(g.seed(), g.cc('pipe-eagain.c'), [ROUNDS, MIB], { env: { GEX_SHARED: '1' }, timeout: 1200000 });
  t.check(`${ROUNDS} rounds of yes | head -c ${MIB}M | wc -c, one and two heads: no EAGAIN, every count exact`,
    r.guest.split('\n').includes(`ok ${ROUNDS} rounds`), `${tail(r.guest, 10)} (${tail(r.stderr, 3)})`);
}
