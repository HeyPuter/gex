// Windows' locks and interlocked calls on objects at addresses that are
// not a multiple of their fields' size, which x86 allows (a split lock)
// (fixtures/wow64/splitlock.c): a critical section, an SRW lock and an
// init-once, each contended by two threads in Wine's own code, which is
// native wasm, whose atomics trap there; and, 32-bit, one thread's locked
// increments in its own translated code racing the other's calls of
// kernel32's InterlockedIncrement on one counter, inside one 8-byte word
// and across two; and, across two, racing its calls on the aligned
// counter that overlaps the counter's upper half. Nothing is lost: each
// count is both threads' rounds, or its own thread's.
// 64-bit and 32-bit, natively and on the page.
import { wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1800 };

const N = 20000;
// (the overlap line's tail, its rounds, varies)
const lines = (out) => out.replace(/\r/g, '').split('\n').filter((l) => l.startsWith('splitlock ')).map((l) => l.replace(/ \(.*\)$/, '')).join('\n');
const want = (mixed) => [
  `splitlock 2 critsec ${2 * N}`, `splitlock 6 critsec ${2 * N}`,
  `splitlock 2 srwlock ${2 * N}`, `splitlock 6 srwlock ${2 * N}`,
  'splitlock 3 initonce 1',
  ...(mixed ? [`splitlock 1 mixed ${2 * N}`, `splitlock 6 mixed ${2 * N}`, 'splitlock 6 overlap kept'] : []),
  'splitlock done',
].join('\n');

export default async function (t) {
  const w = wow64(t);
  for (const [bits, x64] of [['64', true], ['32', false]]) {
    const exe = await w.mingw('splitlock', { x64, out: `splitlock${bits}.exe` });
    const n = await w.native([`C:\\splitlock${bits}.exe`], { files: { [`splitlock${bits}.exe`]: exe }, name: `native-${bits}` });
    t.eq(`native, ${bits}-bit: nothing is lost`, lines(n.out), want(!x64), w.detail(n));
    const p = await w.page([`/t/splitlock${bits}.exe`], { files: { [`/t/splitlock${bits}.exe`]: exe }, name: `page-${bits}` });
    t.eq(`page, ${bits}-bit: nothing is lost`, lines(p.out), want(!x64), w.detail(p));
  }
  w.clean();
}
