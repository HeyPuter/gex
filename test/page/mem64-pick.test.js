// The memory flavour chosen per program, at exec, in the default page whose
// processes have the lowered wasm32 memory: an exec with GEX_MEM64=1 in its
// environment, or of an image whose user.gex.mem64 attribute is 1 (the
// learned record), goes on in a 64-bit memory. The flag applies to that exec
// alone: the kernel takes it out of the new program's environment, so the
// 64-bit program's own children -- by fork and exec, and by posix_spawn --
// are 32-bit again, and so is a sibling exec'd without it. GEX_MEM64=0
// overrides the record. A process tells which memory it has by a mapping
// asked for at 5 GiB: only a 64-bit memory can give it there.
import { guest, tail } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 240 };

const WANT = `parent: 32
asked: 64
its child: 32
its spawned child: 32
sibling: 32
spawned, asked: 64
recorded: 64
spawned, recorded: 64
recorded, asked 32: 32
record gone: 32`;

export default async function (t) {
  const g = guest(t);
  for (const f of ['gex_browser.x86.shared.wasm', 'gex_kernel.x86.wasm', 'gex_browser.x86.m64.shared.wasm', 'seed.x86.m64.shared.wasm']) g.pub(f);
  const prog = g.cc('mem64-pick.c', { imageBase: false });
  const r = await g.run(g.seed(), prog, [], { files: { '/g/pick': prog }, argv0: '/g/pick', env: { GEX_SHARED: '1' }, timeout: 180000 });
  t.eq('the flavour per program at exec, children and siblings 32-bit', r.guest, WANT, tail(r.stderr, 6));
}
