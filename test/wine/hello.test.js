// 64-bit Wine through the runtime's classes, as a page composes it: the
// install at /wine64 and a page prefix at /pfx (fixtures/images.mjs
// `wine64`, its big files lazy), and a console program run from C:\ with
// its arguments and exit code. Wine starts its own server.
import { System } from '../../web/runtime/src/index.js';
import { Filesystem } from '../../web/runtime/src/fs.js';

export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 600 };

export default async function (t) {
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new Filesystem(sys);
  fs.addTar(t.work('wine64.tar'), { lazy: t.work('wine64-lazy.json') });
  fs.addTar(t.work('win-progs.tar'), { at: '/progs' });
  await t.timeout(sys.boot(), 120000, 'boot');
  const env = { WINEPREFIX: '/pfx', WINEDEBUG: 'err+all,fixme-all', WINEGEX_DOOR: '1' };
  const wine = '/wine64/lib/wine/x86_64-unix/wine';
  await fs.writeFile('/pfx/drive_c/hello_args.exe', await fs.readFile('/progs/x64/hello_args.exe'));
  const r = await t.time('hello', () => t.timeout(sys.run([wine, 'C:\\hello_args.exe', 'a', 'b c'], { env, cwd: '/pfx/drive_c' }), 300000, 'hello_args'));
  t.eq('exit', r.code, 42, r.stderr.trim().split('\n').slice(-4).join(' / '));
  t.eq('output', r.stdout.replace(/\r/g, ''), 'hello from a Windows program with 3 args\narg 1: a\narg 2: b c\n');
  await sys.shutdown();
}
