// A 32-bit program through 64-bit Wine's WoW64, as short as a case gets:
// syswow64's cmd.exe echoes a line. The page's own WoW64 case, in the
// browser and in the single-thread build (--single); wine/wow64 has the
// rest of WoW64's cases. Needs a WoW64 Wine install (lib/wine/i386-windows);
// skipped without one.
import { bootWine, WINE } from '../lib/wine.mjs';

export const meta = { hosts: ['browser'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 900, singleTimeout: 1800 };

export default async function (t) {
  const w = await bootWine(t, { wow64: true });
  const r = await t.time('echo', () => t.timeout(w.sys.run([WINE, 'C:\\windows\\syswow64\\cmd.exe', '/c', 'echo', 'wow64-echo'],
    { env: w.env, cwd: '/pfx/drive_c' }), 600000, 'cmd /c echo'))
    .catch((e) => ({ code: -1, stdout: '', stderr: String(e.message) }));
  const out = r.stdout.replace(/\r/g, '');
  // the build that ran it: a page with no SharedArrayBuffer runs the single-thread one
  const build = typeof SharedArrayBuffer === 'function' ? 'shared-memory' : 'single-thread';
  t.check(`syswow64 cmd /c echo (${build} build)`, /^wow64-echo$/m.test(out) && r.code === 0,
    `exit ${r.code}; stdout ${JSON.stringify(out.slice(-300))}; stderr ${JSON.stringify(r.stderr.trim().split('\n').slice(-4).join(' / '))}`);
  await w.sys.shutdown();
}
