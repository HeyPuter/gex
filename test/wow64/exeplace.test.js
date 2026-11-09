// A 64-bit program that can move, in a 64-bit page memory (the arena gex
// gives above 4 GiB, filled top down), goes top down with the DLLs rather
// than at its own base (0x140000000), which cut the arena in two; one with
// no relocations stays at its base. And GlobalMemoryStatusEx's available virtual memory is the free
// address space of the user range (what VirtualQuery shows), not the total
// less the working set; in the page's 64-bit memory that leaves out
// [2 GiB, 4 GiB), gex's own space, which shows as reserved.
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { wow64 } from '../lib/winerun.mjs';

const SRC = fileURLToPath(new URL('../fixtures/wine/src/exeplace.c', import.meta.url));

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

const BASE = 0x140000000n;
const parse = (out) => {
  const m = /^exeplace base (?:0x)?([0-9a-fA-F]+) avail (\d+) total (\d+) free (\d+) largest (\d+) mid ([0-9a-f]+)/m.exec(out);
  return m && { base: BigInt(`0x${m[1]}`), avail: BigInt(m[2]), total: BigInt(m[3]), free: BigInt(m[4]), largest: BigInt(m[5]), mid: parseInt(m[6], 16) };
};
const hex = (n) => `0x${n.toString(16)}`;

export default async function (t) {
  const w = wow64(t);
  const moving = await w.mingw('exeplace', { x64: true, src: SRC, flags: ['-Wl,--dynamicbase,--high-entropy-va'], out: 'exeplace.exe' });
  const fixed = await w.mingw('exeplace', { x64: true, src: SRC, flags: ['-Wl,--disable-dynamicbase,--disable-reloc-section'], out: 'exeplace-fixed.exe' });
  // the avail is the free space, give or take what changed between the two
  // looks (the walk is cached for a second; 64 MiB of slack)
  const truthful = (r) => r.avail < r.total && r.avail + (64n << 20n) >= r.free && r.avail <= r.free + (64n << 20n) && r.largest <= r.free;
  const say = (r) => r && `base ${hex(r.base)}, avail ${r.avail}, free ${r.free}, largest ${r.largest}, total ${r.total}, at 2.25 GiB ${r.mid.toString(16)}`;

  const n = await w.native(['C:\\exeplace.exe'], { files: { 'exeplace.exe': moving }, name: 'native' });
  const nr = parse(n.out);
  t.say(`native: ${say(nr)}`);
  t.check('native: the available virtual memory is the free space', nr && truthful(nr), w.detail(n));

  // the page legs: the memory64 twin of the loader and modules (WINE64_TWIN=1),
  // which the exec of a program asked one (GEX_MEM64=1), or only loadable in
  // one, runs (600 s: the twin's first start compiles its modules)
  if (!existsSync(join(process.env.GEX_TEST_WOW64_DIST, 'lib/wine/x86_64-unix/wine.m64'))) t.skip('the WoW64 install has no memory64 twin (WINE64_TWIN=1)');
  const p = await w.page(['/t/exeplace.exe'], { files: { '/t/exeplace.exe': moving }, name: 'page-m64', timeout: 600000, wineEnv: { GEX_MEM64: '1' } });
  const pr = parse(p.out);
  t.say(`page mem64: ${say(pr)}`);
  t.check('page mem64: a program that can move goes top down, above its base', pr && pr.base > BASE && pr.base >= 1n << 32n, w.detail(p));
  t.check('page mem64: the available virtual memory is the free space', pr && truthful(pr), w.detail(p));
  t.check('page mem64: gex\'s space (2 to 4 GiB) is reserved and out of the available', pr && pr.mid === 0x2000 && pr.avail + (2n << 30n) <= pr.total, w.detail(p));
  const f = await w.page(['/t/exeplace-fixed.exe'], { files: { '/t/exeplace-fixed.exe': fixed }, name: 'page-m64-fixed', timeout: 600000 });
  const fr = parse(f.out);
  t.say(`page mem64, no relocations: ${say(fr)}`);
  t.check('page mem64: a program with no relocations stays at its base', fr?.base === BASE, w.detail(f));
  w.clean();
}
