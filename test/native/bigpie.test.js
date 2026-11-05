// A position-independent executable too big to place below the interpreter
// (fixtures/native/bigpie: 96 MiB of .bss) runs through its interpreter
// (gex_ld::Program::direct): argv[0], the arguments and /proc/self/exe are
// the program's, in the process gexc starts and in one it execs. An aarch64
// musl program, natively.
import { copyFileSync, mkdirSync } from 'node:fs';
import { native, trim, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 600 };

export default async function (t) {
  const n = native(t);
  const d = n.dir('bigpie');
  mkdirSync(`${d}/root/lib`, { recursive: true });
  mkdirSync(`${d}/root/proc`, { recursive: true });
  const [[ld, libc]] = Object.entries(n.ldso);
  await n.cc(['-O1', '-fPIE', '-pie', '-o', `${d}/root/bigpie`, n.src('bigpie/bigpie.c')]);
  copyFileSync(libc, `${d}/root${ld}`);
  const r = await n.gexc(['run', '--sysroot', `${d}/root`, `${d}/root/bigpie`], { timeout: 300000 });
  // (gexc gives the program the path it was named by, as argv[0])
  t.eq('output', trim(r.stdout + r.stderr), `parent: argv[0] ${d}/root/bigpie, 1 args, exe /bigpie, ends 1 2\nchild: argv[0] renamed, 2 args, exe /bigpie, ends 1 2\nchild exit 0`);
  t.check('exits 0', r.code === 0, `exit ${r.code}: ${tail(r.stderr)}`);
  n.clean();
}
