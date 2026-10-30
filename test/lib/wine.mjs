// 64-bit Wine composed from the runtime's classes, as a page does it (images
// `wine64` and `win-progs`): its install at /wine64, the page prefix at /pfx,
// the test programs at /progs/{x64,x86}. Then `run(exe, args)` copies a test
// program into C:\ and runs it with Wine: `{ code, stdout, stderr }`, the
// output's line ends Unix ones.
import { System } from '../../web/runtime/src/index.js';
import { Filesystem } from '../../web/runtime/src/fs.js';

export const WINE = '/wine64/lib/wine/x86_64-unix/wine';

/** `env`: more of Wine's environment; `parts(sys, fs)`: the page's other
 *  classes, made before boot. */
export async function bootWine(t, { env = {}, parts } = {}) {
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new Filesystem(sys);
  fs.addTar(t.work('wine64.tar'), { lazy: t.work('wine64-lazy.json') });
  fs.addTar(t.work('win-progs.tar'), { at: '/progs' });
  const more = parts?.(sys, fs);
  await t.timeout(sys.boot(), 120000, 'boot');
  const failed = JSON.parse(await (await fetchWork(t, 'win-progs.json')).text()).failed;
  const wineEnv = { WINEPREFIX: '/pfx', WINEDEBUG: 'err+all,fixme-all', WINEGEX_DOOR: '1', ...env };
  /** Run test program `exe` (in /progs/ARCH) from C:\. */
  async function run(exe, args = [], { arch = 'x64', timeout = 300000, env: e = {} } = {}) {
    const why = failed[`${arch}/${exe.replace(/\.exe$/, '.c')}`] ?? failed[`${arch}/${exe.replace(/\.exe$/, '.cpp')}`];
    if (why) throw new Error(`${arch}/${exe} did not build: ${why}`);
    await fs.writeFile(`/pfx/drive_c/${exe}`, await fs.readFile(`/progs/${arch}/${exe}`));
    const r = await t.timeout(sys.run([WINE, `C:\\${exe}`, ...args], { env: { ...wineEnv, ...e }, cwd: '/pfx/drive_c' }), timeout, exe);
    return { ...r, stdout: r.stdout.replace(/\r/g, '') };
  }
  return { sys, fs, run, env: wineEnv, more };
}

function fetchWork(t, name) {
  const u = t.work(name);
  if (u.startsWith('file:')) return import('node:fs/promises').then((f) => f.readFile(new URL(u))).then((b) => new Response(b));
  return fetch(u);
}
