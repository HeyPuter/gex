// The function table's probe cache in a 64-bit memory (gex_abi::FCACHE): two
// functions 4 GiB apart share a slot (the slot hashes the low 32 bits) and
// their low 32 bits. Called in turn through pointers from the function JIT's
// code, each call must reach its own function: an entry that kept only the
// low 32 bits sent every call at t to the function at t + 4 GiB (and a 64-bit
// memory's images, near the top of its 16 GiB, never hit at all).
import { guest, tail } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 240 };

export default async function (t) {
  const g = guest(t);
  g.pub('gex_browser.x86.m64.shared.wasm');
  const seed = g.pub('seed.x86.m64.shared.wasm');
  const prog = g.cc('mem64-fcache.c', { imageBase: false });
  const r = await g.run(seed, prog, [], { files: { '/g/fc': prog }, argv0: '/g/fc', env: { GEX_SHARED: '1', GEX_MEM64: '1', GEX_FJIT: '1' }, timeout: 180000 });
  const got = r.guest.split('\n').filter((l) => /^(ok|FAIL)/.test(l)).join(' / ');
  t.check(`the probe cache in a 64-bit memory: ${got || 'no result'}`, /^ok:/.test(got), tail(r.stderr, 6));
}
