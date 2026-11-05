// The conformance suite (fixtures/integration/suite.c: arith, float, mem,
// flow, sys) under gexc and QEMU (lib/qemu.mjs): static, static-PIE (its
// R_AARCH64_RELATIVE relocations) and static at the fixed 0x400000 base, -O0
// (QEMU 7.2 dies on its longjmp case before any output, so a host-native build
// stands in there), with --no-jit (the translate-everything-up-front path),
// and dynamically linked against the toolchain's musl, libstdc++ and libgcc_s
// with the vector, thread and C++ programs beside it: the guest's own loader
// translated, a whole libc through runtime translation, threads racing to
// translate library code, and a throw unwinding across a DSO.
import { join } from 'node:path';
import { PROGRAMS, qemu } from '../lib/qemu.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 900 };

const AREAS = ['arith', 'float', 'mem', 'flow'];

export default async function (t) {
  const q = qemu(t);
  const suite = await q.build('suite', 'suite.c', ['-static', '-no-pie', '-O2']);
  const pie = await q.build('suite-pie', 'suite.c', ['-static', '-O2']);
  const o0 = await q.build('suite-O0', 'suite.c', ['-static', '-no-pie', '-O0']);
  const hostSuite = join(q.dir, 'suite-native');
  await q.n.hostcc(['-O0', '-o', hostSuite, join(PROGRAMS, 'suite.c')]);
  for (const a of AREAS) await q.compare(`static-${a}`, suite, [a]);
  // The sys area writes to stderr ("stderr-works").
  await q.compare('static-sys', suite, ['sys'], { stderr: true });
  for (const a of AREAS) {
    await q.compare(`pie-${a}`, pie, [a]);
    await q.compare(`O0-${a}`, o0, [a], { oracle: hostSuite });
  }
  for (const a of AREAS) await q.compare(`nojit-${a}`, suite, [a], { gexc: ['--no-jit'] });

  const root = q.sysroot();
  const dyn = { gexc: ['--sysroot', root], qemu: ['-L', root] };
  const dsuite = await q.build('dyn-suite', 'suite.c', ['-O2']);
  const dvector = await q.build('dyn-vector', 'vector.c', ['-O2']);
  const dthreads = await q.build('dyn-threads', 'dyn-threads.c', ['-O2', '-pthread', '-lm']);
  const dcpp = await q.buildCxx('dyn-cpp', 'cpp.cc', ['-O2']);
  for (const a of AREAS) await q.compare(`dynamic-${a}`, dsuite, [a], dyn);
  await q.compare('dynamic-vector', dvector, [], dyn);
  await q.compare('dynamic-threads', dthreads, [], dyn);
  await q.compare('dynamic-cpp', dcpp, [], dyn);
}
