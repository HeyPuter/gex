// The translated-module registry of a threaded process keeps its bytes in the
// process's JS-side module store (`Gex#moduleStore`), not in the shim's heap.
// The registry is what a thread Worker that joins later replays to catch up.
// It used to hold every module's bytes in the shim's heap, which shares a
// fixed 2 GiB arena with the guest's low mappings; in a long fjit run it
// reached 980 MiB and the heap could not grow. Here a threaded guest
// generates and runs a few thousand distinct functions, each a fresh
// translation the registry keeps, then starts one more thread, which has to
// catch up on all of them. The store must have grown by megabytes while the
// registry's bytes in the heap stay at what was lifted before the first
// thread (about one MiB), and the store may grow past 4 GiB.
import { execFileSync } from 'node:child_process';
import { guest, tail } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 360 };

export default async function (t) {
  const g = guest(t);
  g.pub('gex_browser.x86.m64.shared.wasm');
  const seed = g.pub('seed.x86.m64.shared.wasm');
  const prog = g.cc('jit-registry.c', { imageBase: false, opt: '-O1' });
  const native = execFileSync(prog, { encoding: 'utf8' }).trim();
  const r = await g.run(seed, prog, [], { env: { GEX_SHARED: '1', GEX_MEM64: '1', GEX_REGISTRY_TRACE: '1' }, timeout: 300000 });
  t.eq('the output is the native run\'s', r.guest, native, tail(r.stderr, 4));
  // The largest of each: Workers' console lines arrive in no particular order.
  const lines = [...r.stderr.matchAll(/module store at (\d+) MiB; registry bytes in the shim heap: (\d+) MiB/g)];
  const store = Math.max(...lines.map((m) => Number(m[1])));
  const heap = Math.max(...lines.map((m) => Number(m[2])));
  t.check(`module store ${store} MiB, registry in the shim heap ${heap} MiB`, lines.length && store >= 2 && heap <= 2);
  const max = Number(/module store max (\d+) GiB/.exec(r.stderr)?.[1]);
  t.check(`the module store may grow to ${max} GiB, past 4`, max > 4);
}
