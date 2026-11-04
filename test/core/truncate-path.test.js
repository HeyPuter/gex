// truncate(2) by name, shorter and longer, on a file a shared mapping holds,
// from each ABI's own call: x86-64's and aarch64's (static programs) and
// wasm32's (i386 numbers, truncate64) (test/fixtures/programs/truncate-path.c).
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';

export const meta = { hosts: ['browser', 'node'], images: ['busybox', 'native-progs'], timeout: 120 };

const WANT = `shorter: 0 4196
mapped: a 0
longer: 0 12288
read: 12288 a zeros
mapped: 0
missing: -1 No such file or directory
directory: -1 Is a directory`;

export default async function (t) {
  const { eq, timeout } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  fs.addFiles({ '/g/truncate-path': new URL(t.work(`truncate-path.${t.arch === 'x86' ? 'x86' : 'a64'}`)) }, { mode: 0o755 });
  await timeout(sys.boot(), 60000, 'boot');
  const env = { PATH: '/bin', HOME: '/' };
  for (const [what, prog] of [['the native program', '/g/truncate-path'], ['wasm32', '/bin/rt-truncate']]) {
    const r = await timeout(sys.run([prog], { env }), 60000, prog);
    eq(`${what}: shorter and longer by name, mapped`, r.stdout.trim(), WANT, r.stderr.slice(-2000));
  }
  await sys.shutdown();
}
