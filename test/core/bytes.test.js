// The page lets the shim's and the kernel's bytes go once they are compiled:
// after boot and two programs, the runtime's module state holds neither
// (runtime.js `heldBytes`), and a second program still starts from the
// host's compiled shim.
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';
import { heldBytes } from '../../page/src/runtime.js';

export const meta = { hosts: ['browser', 'node'], images: ['busybox'], timeout: 120 };

export default async function (t) {
  const { eq, timeout } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  await timeout(sys.boot(), 60000, 'boot');
  const env = { PATH: '/bin:/usr/bin' };
  for (const n of [1, 2]) eq(`run ${n}`, (await timeout(sys.run(['echo', `hi ${n}`], { env }), 30000, `run ${n}`)).stdout, `hi ${n}\n`);
  eq('the bytes the runtime holds after compiling', heldBytes(), { shim: false, kernel: false });
  await sys.shutdown();
}
