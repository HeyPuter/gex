// Message passing between two guest threads, with plain x86 `mov`s, keeps
// x86 order in the TSO flavour (GEX_TSO=1, the page's `?tso=1`). Thread A
// writes data[i] then flag[i]; thread B reads flag[i] then data[i]. x86 never
// lets B see the flag without the data. Translated to plain wasm accesses,
// V8 may reorder them (rarely on an x86 host, freely on arm64); in the TSO
// flavour they are atomic and must not. The plain run's count is said for
// information (nonzero on an arm64 host is expected).
import { guest } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1260 };

const ROUNDS = 300;

export default async function (t) {
  const g = guest(t);
  g.pub('gex_browser{infix}.shared.wasm');
  g.pub('gex_kernel{infix}.wasm');
  const prog = g.cc('tso-mp.c');
  const seed = g.seed();
  const mp = async (tso) => {
    const r = await g.run(seed, prog, [ROUNDS], { env: { GEX_SHARED: '1', GEX_TSO: tso }, timeout: 600000 });
    return r.guest.split('\n').find((l) => l.startsWith('mp ')) ?? 'mp ? (the run failed)';
  };
  t.say(`plain: ${await mp('0')}`);
  const got = await mp('1');
  t.check(`tso: ${got} reordered`, /^mp 0 of /.test(got) && !/ of 0$/.test(got), / of 0$/.test(got) ? 'the reader never saw a flag' : '');
}
