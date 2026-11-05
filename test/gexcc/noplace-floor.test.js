// A native wasm32 program has no placement window: its arena starts at
// MMAP_BOTTOM (112 MiB), not above the 64 MiB window translated programs
// place shared objects in. Checked by the memory each process has when its
// main runs (fixtures/gexcc/noplace-floor/w.c, gexcc's: the program's own, a
// fork child's, a posix_spawn child's on a fresh memory) against a floor
// below what the window costs, on the page's shim. A translated program the
// wasm32 one starts (x.c: posix_spawn, and an execv from a wasm32 child,
// which goes on in a memory with the window) still has it: its first mmap
// is above the window. The wasm32 program's image lies where the window was
// (0x08048000): a large bss there survives the arena growing past it.
// Floors: 150 MiB for a fresh process, 160 a fork child's (its parent's copy
// plus its own runtime's first claims). With the window: 247 MiB, 303
// forked, 231 spawned; without it, 206, 262 and 190; with small first heap
// spans, 177, 218 and 176; with no slot map, 139, 142 and 138.
import { join } from 'node:path';
import { gexcc, FIXTURES } from '../lib/gexcc.mjs';
import { guest, tail } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 600 };

const FLOOR = { first: 150, forked: 160, spawned: 150 };

export default async function (t) {
  const c = gexcc(t);
  const g = guest(t);
  const seed = g.pub('seed.x86.shared.wasm');
  const d = c.dir('noplace-floor');
  await c.build(['-O1', '-o', join(d, 'w'), join(FIXTURES, 'noplace-floor/w.c')]);
  await c.hostBuild(['-O1', '-static', '-o', join(d, 'x'), join(FIXTURES, 'noplace-floor/x.c')]);
  const r = await g.run(seed, join(d, 'w'), [], { files: { '/t/w': join(d, 'w'), '/t/x': join(d, 'x') }, argv0: '/t/w', flags: ['--cwd', '/t'], env: { GEX_LOG: 'off', GEX_SHARED: '1' }, timeout: 300000 });
  const out = r.stdout.split('\n').filter((l) => !/^\[(host|gex|worker)\] |^\[gex-console/.test(l));
  const value = (re) => out.map((l) => re.exec(l)?.[1]).find((v) => v !== undefined);
  for (const [who, most] of Object.entries(FLOOR)) {
    const m = value(new RegExp(`^${who} memory (\\d+)$`));
    t.check(`${who}: memory at main at most ${most} MiB (${m ?? 'no line'})`, m !== undefined && Number(m) <= most, tail(out.join('\n')));
  }
  t.check('image intact', out.includes('image intact'), out.find((l) => l.startsWith('image')) ?? 'no line');
  // the window is 112-176 MiB: a translated program's arena starts above it
  for (const how of ['spawn', 'exec']) {
    const a = value(new RegExp(`^x86 ${how} mmap (\\d+)$`));
    t.check(`x86 by ${how}: first mmap above the window (${a ?? 'no line'} MiB)`, a !== undefined && Number(a) >= 176, tail(r.stderr, 2));
  }
  c.clean();
}
