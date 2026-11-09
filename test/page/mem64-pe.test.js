// The memory flavour an exec of Wine's loader gets from the Windows
// program it names (WINEGEX_IMAGE), in the default page whose processes
// have the lowered wasm32 memory: an x86-64 image with no relocations whose
// preferred base is above the lowered memory's code space (a linker's
// default 0x140000000) can only be loaded in a 64-bit memory, so its exec
// goes on in one, with nothing asked or recorded. A relocatable image, a
// low one, an i386 one or a missing file keep the page's; GEX_MEM64=0
// overrides. Wine's loader started by hand on a DOS path (no
// WINEGEX_IMAGE) is judged by that program, found through WINEPREFIX's
// dosdevices.
// Synthetic PE headers the guest writes itself.
import { guest, tail } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 240 };

const WANT = `parent: 32
fixed high: 64
no reloc directory: 64
relocatable: 32
fixed low: 32
not x86-64: 32
fixed high, asked 32: 32
missing: 32
by hand, fixed high: 64
by hand, relocatable: 32`;

export default async function (t) {
  const g = guest(t);
  for (const f of ['gex_browser.x86.shared.wasm', 'gex_kernel.x86.wasm', 'gex_browser.x86.m64.shared.wasm', 'seed.x86.m64.shared.wasm']) g.pub(f);
  const prog = g.cc('mem64-pe.c', { imageBase: false });
  const r = await g.run(g.seed(), prog, [], { files: { '/g/pe': prog }, argv0: '/g/pe', env: { GEX_SHARED: '1' }, timeout: 180000 });
  t.eq('the flavour from the Windows image at exec', r.guest, WANT, tail(r.stderr, 6));
}
