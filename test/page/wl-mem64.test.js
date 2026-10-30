// A memory64 process's wl_shm pool above 4 GiB reaches the page's compositor
// whole (fixtures/programs/mem64-wl.c): a raw client maps a toplevel from a
// pool at 0x140000000 and prints the pixels' sum; the headless compositor
// (GEX_WL_STUB, GEX_WL_STUB_SUM) sums what it reads at the surface's
// address. The address used to cross as 32 bits (gex-wayland
// `Buffers::locate`), so the window was never published -- or would have
// been drawn from the low alias, the shim's heap.
import { guest, tail } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 120 };

export default async function (t) {
  const g = guest(t);
  const seed = g.pub('seed.x86.m64.shared.wasm');
  const prog = g.cc('mem64-wl.c', { imageBase: false, opt: '-O1' });
  const r = await g.run(seed, prog, [], { env: { XDG_RUNTIME_DIR: '/tmp', GEX_WL_STUB: '1', GEX_WL_STUB_SUM: '1', GEX_SHARED: '1', GEX_MEM64: '1' }, timeout: 60000 });
  const all = `${r.stdout}\n${r.stderr}`;
  const pool = /^pool at (\S+)/m.exec(all)?.[1];
  const sum = /^sum=(\S+)/m.exec(all)?.[1];
  // The last publish of the 64x64 window: the one after its pixels were in.
  const seen = [...all.matchAll(/\[wlstub\] window 64x64 at .*/g)].pop()?.[0];
  t.check(`the pool above 4 GiB (${pool})`, r.code === 0 && /^0x[1-9][0-9a-f]{8,}$/.test(pool ?? ''), tail(all, 6));
  t.eq('the compositor read the pool whole', seen, `[wlstub] window 64x64 at ${pool} sum=${sum}`);
}
