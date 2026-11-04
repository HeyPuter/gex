// The package page/runtime/build.sh makes (run.mjs builds it into the work
// directory), imported as a page would, with no baseUrl: it finds its files
// itself.

export const meta = { hosts: ['browser'], images: ['busybox', 'package'], timeout: 120 };

export default async function (t) {
  const { say, eq, timeout } = t;
  // Not literals, which vite would try to resolve as its own.
  const pkg = t.work('pkg/');
  const { System } = await import(/* @vite-ignore */ `${pkg}index.mjs`);
  const { MemoryFilesystem } = await import(/* @vite-ignore */ `${pkg}fs.mjs`);
  const sys = new System();
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  const t0 = performance.now();
  await timeout(sys.boot(), 60000, 'boot');
  say(`booted in ${(performance.now() - t0).toFixed(0)} ms`);
  const r = await timeout(sys.run(['echo', 'from the package'], { env: { PATH: '/bin' } }), 30000, 'run');
  eq('run', [r.code, r.stdout], [0, 'from the package\n']);
  await sys.shutdown();
}
