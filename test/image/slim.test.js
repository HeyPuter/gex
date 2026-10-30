// A Debian image (run.mjs RT_SLIM: debdemo's slim wasm32 image and its
// repository): boot it, apt install from the repository mounted from the
// page, reload, and the package is still installed (IdbPersist).
import { System } from '../../web/runtime/src/index.js';
import { Filesystem } from '../../web/runtime/src/fs.js';
import { IdbPersist } from '../../web/runtime/src/persist/idb.js';
import { HttpFs } from '../fixtures/httpfs.js';

export const meta = { hosts: ['browser'], images: ['slim'], timeout: 180 };

const DB = 'rt-slim-test';
const PKG = 'xcvt';
const CMD = 'cvt';

export default async function (t) {
  const { say, check, eq, timeout } = t;
  const phase = t.phase;
  const list = await (await fetch(t.work('slim-repo.json'))).json();
  if (!list) return say('skipped: no image (RT_SLIM)');
  if (phase === 1) await new Promise((r, j) => { const q = indexedDB.deleteDatabase(DB); q.onsuccess = r; q.onerror = j; });
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new Filesystem(sys);
  let start = performance.now();
  fs.addTar(t.work('slim/debian-slim.tar.zst'));
  fs.mount('/mnt/repo', new HttpFs(t.work('slim/repo'), list));
  fs.addFiles({ '/etc/apt/sources.list.d/gex.list': 'deb [trusted=yes arch=musl-linux-wasm32,arm64] file:/mnt/repo trixie main\n' });
  const persist = new IdbPersist(fs, { db: DB });
  await timeout(sys.boot(), 120000, 'boot');
  say(`phase ${phase}: booted in ${(performance.now() - start).toFixed(0)} ms (${JSON.stringify(sys.describe().arch)})`);
  const env = { PATH: '/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin', HOME: '/root', DEBIAN_FRONTEND: 'noninteractive' };
  const run = async (argv, ms = 240000) => {
    const t0 = performance.now();
    const r = await timeout(sys.run(argv, { env }), ms, argv.join(' '));
    say(`${argv.join(' ')}: exit ${r.code} in ${(performance.now() - t0).toFixed(0)} ms`);
    if (r.code) say(`  ${(r.stderr || r.stdout).slice(-800)}`);
    return r;
  };

  // The image's own start (its D-Bus system bus), run by the page.
  const boot = await timeout(sys.spawn(['/etc/gex/boot'], { env, detached: true }).exited, 60000, '/etc/gex/boot');
  eq('/etc/gex/boot', boot, { code: 0, signal: null });
  if (phase === 1) {
    eq('dbus started by /etc/gex/boot', (await run(['sh', '-c', 'test -S /run/dbus/system_bus_socket && echo up'])).stdout, 'up\n');
    // Offline: only the mounted repository.
    await run(['rm', '-f', '/etc/apt/sources.list.d/debian.sources']);
    eq('apt-get update', (await run(['apt-get', 'update'])).code, 0);
    eq(`apt-get install ${PKG}`, (await run(['apt-get', 'install', '-y', PKG])).code, 0);
    eq(`${PKG} runs`, (await run([CMD, '1920', '1080'])).stdout.includes('Modeline'), true);
    // ps and pstree with pid 1 the kernel's.
    const ps = await run(['ps', '-eo', 'pid,ppid,comm']);
    check('ps lists pid 1 and the page\'s programs under it', ps.code === 0 && /^\s*1\s+0\s+init$/m.test(ps.stdout) && /^\s*\d+\s+1\s+ps$/m.test(ps.stdout), ps.stdout + ps.stderr);
    eq('apt-get install psmisc', (await run(['apt-get', 'install', '-y', 'psmisc'])).code, 0);
    const tree = await run(['pstree', '-p']);
    check('pstree', tree.code === 0 && /^init\(1\).*pstree\(\d+\)/s.test(tree.stdout), tree.stdout + tree.stderr);
    start = performance.now();
    const s = await persist.save();
    say(`saved ${s.stored} records (${s.bytes} bytes) in ${(performance.now() - start).toFixed(0)} ms; in all ${JSON.stringify(persist.stats)}`);
    await sys.shutdown();
    return t.reload();
  }

  eq(`${PKG} is still installed`, (await run(['dpkg-query', '-W', '-f', '${Status}', PKG])).stdout, 'install ok installed');
  eq(`${PKG} still runs`, (await run([CMD, '1920', '1080'])).stdout.includes('Modeline'), true);
  eq('apt still knows the repository', (await run(['apt-cache', 'policy', PKG])).stdout.includes('file:/mnt/repo'), true);
  await persist.reset();
  await sys.shutdown();
}
