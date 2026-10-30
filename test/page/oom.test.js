// A process out of memory fails cleanly. The engine refuses a grow (Windows
// commits a wasm memory as it grows, so a page with many large processes
// meets this); here the memory is capped instead (GEX_MEM_MAX_MIB, 320 MiB),
// which fails a grow the same way. The guest allocates until malloc says no
// -- big blocks (mmap), small ones (brk), and both from four threads at once
// -- and must see NULL/ENOMEM and exit on its own: no trap, no out-of-bounds
// access, no hang. And the kernel's memory, capped too (GEX_KERNEL_MAX_MIB,
// 768 MiB, room for its 256 MiB of Worker stacks and the root): a file
// filled in its filesystem until a write fails, after which a new process
// must still run (no kernel lock left held).
import { guest } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1260 };

const CAP = 320;
const KCAP = 768;

export default async function (t) {
  const g = guest(t);
  g.pub('gex_browser{infix}.shared.wasm');
  g.pub('gex_kernel{infix}.wasm');
  const prog = g.cc('oom.c');
  const seed = g.seed();
  for (const mode of ['big', 'small', 'threads', 'tmpfs']) {
    const cap = mode === 'tmpfs' ? KCAP : CAP;
    const r = await g.run(seed, prog, [mode, cap], { env: { GEX_SHARED: '1', GEX_MEM_MAX_MIB: String(CAP), GEX_KERNEL_MAX_MIB: String(KCAP) }, timeout: 300000 });
    const line = r.guest.split('\n').find((l) => l.startsWith(`ok ${mode} `));
    const why = r.signal === 'SIGKILL' ? 'hung (timeout)' : `exit ${r.code}`;
    const trouble = r.stderr.split('\n').filter((l) => /out of|threw|RangeError|bounds|unreachable|panick/.test(l)).slice(0, 4).join(' / ');
    t.check(`oom/${mode} (cap ${cap} MiB)${line ? `: ${line.split(' ').slice(2).join(' ')} MiB before it failed` : ''}`, r.code === 0 && !!line, `${why}: ${r.guest.split('\n').slice(0, 3).join(' / ')} ${trouble}`);
  }
}
