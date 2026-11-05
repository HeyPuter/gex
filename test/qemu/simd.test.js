// AdvSIMD, floating point and control flow under gexc and QEMU (lib/qemu.mjs):
// what gcc's vectoriser picks at -O3 (static and PIE), -ffast-math's
// by-element multiplies and widening conversions, structure loads and
// glibc's string-routine shapes through intrinsics, the fixed-point
// conversions (inline assembly: no C spelling), hand-written cross-block flag
// liveness (a condition set in one block and used in another), loops of every
// shape, and fused multiply-add compared as bit patterns. The second half runs
// with --no-jit, as do the last of the first's programs.
import { qemu } from '../lib/qemu.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 900 };

export default async function (t) {
  const q = qemu(t);
  const b = (name, src, flags) => q.build(name, src, flags);
  const vector = await b('vector', 'vector.c', ['-static', '-no-pie', '-O3']);
  const vectorPie = await b('vector-pie', 'vector.c', ['-static', '-O3']);
  const numeric = await b('numeric', 'numeric.c', ['-static', '-no-pie', '-O3', '-ffast-math', '-lm']);
  const neon2 = await b('neon2', 'neon2.c', ['-static', '-no-pie', '-O2']);
  // -O2: these are intrinsics, and -O3 lets gcc constant-fold half of them away.
  const neon3 = await b('neon3', 'neon3.c', ['-static', '-no-pie', '-O2']);
  const varshift = await b('neon-varshift', 'neon-varshift.c', ['-static', '-no-pie', '-O2']);
  const move = await b('neon-move', 'neon-move.c', ['-static', '-no-pie', '-O2']);
  const flags = await b('flags', 'flags.S', ['-nostdlib', '-static', '-no-pie']);
  const loops = await b('loops', 'loops.c', ['-O2', '-static']);
  const fixed = await b('fixedpoint', 'fixedpoint.c', ['-static', '-no-pie', '-O2']);
  const more = {};
  for (const p of ['neon-cmp64', 'neon-lane-stride', 'neon-fixedconv', 'neon-interleave', 'neon-clz', 'neon-pairwise', 'fp-literal', 'fma', 'frinta']) more[p] = await b(p, `${p}.c`, ['-O2', '-static']);
  more['neon-mul-rbit'] = await b('neon-mul-rbit', 'neon-mul-rbit.c', ['-O3', '-static']);

  await q.compare('vector-O3', vector);
  await q.compare('vector-pie', vectorPie);
  await q.compare('numeric', numeric);
  await q.compare('neon-structure', neon2);
  // The narrowing adds, the pairwise reductions and the read-modify-write
  // forms of the modified-immediate group: what glibc's string routines use.
  await q.compare('neon-glibc', neon3);
  await q.compare('neon-varshift', varshift);
  await q.compare('neon-move', move);
  await q.compare('cross-block-flags', flags);
  await q.compare('loops', loops);
  await q.compare('fixed-point', fixed);

  const nojit = { gexc: ['--no-jit'] };
  for (const [p, prog] of Object.entries(more)) await q.compare(p, prog, [], nojit);
  await q.compare('nojit-vector', vector, [], nojit);
  await q.compare('nojit-neon-structure', neon2, [], nojit);
  await q.compare('nojit-cross-block-flags', flags, [], nojit);
  await q.compare('nojit-loops', loops, [], nojit);
  await q.compare('nojit-fixed-point', fixed, [], nojit);
}
