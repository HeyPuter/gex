// A system on an x86 guest runs a program on a 64-bit memory when its exec
// asks (GEX_MEM64=1) or its image's user.gex.mem64 record says so: the
// memory64 shim and seed the package ships, fetched then. The program is
// page/mem64-pick's (test/fixtures/programs/mem64-pick.c).
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';

export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['busybox', 'mem64'], timeout: 120 };

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
  const { eq, timeout } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  fs.addFiles({ '/g/pick': new URL(t.work('mem64-pick.x86')) }, { mode: 0o755 });
  await timeout(sys.boot(), 60000, 'boot');
  const r = await timeout(sys.run(['/g/pick'], { env: { PATH: '/bin', HOME: '/' } }), 60000, 'mem64-pick');
  eq('the flavour per program at exec, children and siblings 32-bit', r.stdout.trim(), WANT, r.stderr.slice(-2000));
  await sys.shutdown();
}
