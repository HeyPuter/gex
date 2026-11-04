// An exec that asks for a 64-bit memory on a page whose memory64 runtime is
// missing fails with ENOENT while it still can, as an exec whose interpreter
// is missing does: a fork child's execve, posix_spawn and the process's own
// execve, which goes on in its old program. The page says why once, on the
// console. In the browser, the package (page/runtime/build.sh) copied with its
// memory64 shim deleted (the package image's pkg-no-m64), imported as a page
// would; on the Node host, which runs the sources, a site laid out as gex's
// without it.
export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['busybox', 'mem64', 'package'], timeout: 180 };

const WANT = `forked execve: No such file or directory
its exit: 3
posix_spawn: No such file or directory
execve: No such file or directory`;

export default async function (t) {
  const { eq, timeout } = t;
  const browser = typeof document !== 'undefined';
  let MemoryFilesystem;
  let sys;
  if (browser) {
    // Not literals, which vite would try to resolve as its own.
    const pkg = t.work('pkg-no-m64/');
    const { System } = await import(/* @vite-ignore */ `${pkg}index.mjs`);
    ({ MemoryFilesystem } = await import(/* @vite-ignore */ `${pkg}fs.mjs`));
    sys = new System({ debug: t.debug() });
  } else {
    const { System } = await import('../../page/runtime/src/index.js');
    ({ MemoryFilesystem } = await import('../../page/runtime/src/fs.js'));
    sys = new System({ baseUrl: `${t.work('no-m64')}/`, debug: t.debug() });
  }
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  fs.addFiles({ '/g/miss': new URL(t.work('mem64-miss.x86')) }, { mode: 0o755 });
  await timeout(sys.boot(), 60000, 'boot');
  const r = await timeout(sys.run(['/g/miss'], { env: { HOME: '/' } }), 60000, 'mem64-miss');
  eq('each exec fails with ENOENT', r.stdout.trim(), WANT, r.stderr.slice(-2000));
  eq('and the program goes on to its end', r.code, 0);
  const said = browser ? await t.logs(/^error .*memory flavour/, { levels: true }) : await t.logs(/memory flavour/);
  eq('said once', said.length, 1, JSON.stringify(said));
  await sys.shutdown();
}
