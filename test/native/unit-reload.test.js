// A unit's library unloaded and another loaded at its base while a second
// thread sleeps, then that thread calls in through another unit (main.c), on
// the page's shim (x86, the shared build, the host's gcc and glibc). Before
// cgen::on_retire cleared a retired unit's probe cache entries, the thread's
// Worker, its table still holding the old unit's body, entered that body
// from the cache, and the body, its page patched, left through the same
// entry to itself: the run hung (Windows 7's uxtheme.dll reloaded in
// explorer /desktop, its DllMain spinning with the loader lock held). The
// "function tier for" lines are debug output.
import { native, exec, count, trim } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 600 };

export default async function (t) {
  const n = native(t);
  n.g.pub('gex_browser.x86.shared.wasm');
  const glibc = n.g.glibc();
  const d = n.dir('unit-reload');
  const gcc = (args) => exec([n.g.tool('gcc'), ...args], { cwd: d }).then((r) => {
    if (r.code !== 0) throw new Error(`gcc ${args.join(' ')}: ${r.stderr.trim()}`);
  });
  await gcc(['-O1', '-shared', '-fPIC', '-DK=3', '-DC=1', '-o', `${d}/liba.so`, n.src('unit-reload/lib.c')]);
  await gcc(['-O1', '-shared', '-fPIC', '-DK=7', '-DC=-2', '-o', `${d}/libb.so`, n.src('unit-reload/lib.c')]);
  // A real call through the pointer (not a tail jump), so it returns into the unit.
  await gcc(['-O1', '-fno-optimize-sibling-calls', '-shared', '-fPIC', '-Wl,-soname,libcall.so', '-o', `${d}/libcall.so`, n.src('unit-reload/call.c')]);
  await gcc(['-O1', '-pthread', '-o', `${d}/main`, n.src('unit-reload/main.c'), `-L${d}`, '-lcall', `-Wl,-rpath,${d}`, '-ldl']);
  const b = await n.gexc(['build', `${d}/main`, '-o', `${d}/main.wasm`]);
  if (b.code !== 0) throw new Error(`gexc build: ${(b.stdout + b.stderr).trim().split('\n').slice(-5).join(' / ')}`);
  const native_ = trim((await exec([`${d}/main`, d])).stdout);
  const cache = `${d}/cgen`;
  const files = { ...glibc, ...Object.fromEntries(['main', 'liba.so', 'libb.so', 'libcall.so'].map((f) => [`${d}/${f}`, `${d}/${f}`])) };
  const run = () => n.g.run(`${d}/main.wasm`, `${d}/main`, [d], { files, flags: ['--cgen-prefer'], cwd: d, timeout: 120000,
    env: { GEX_CGEN_CACHE: cache, GEX_ELF_UNITS: 'lib.so,libcall.so', GEX_SHARED: '1', GEX_LOG: '1' } });
  // A first run (no units yet) finds where the loader maps the two libraries
  // (liba.so's soname is lib.so).
  const err = (await run()).stderr;
  const ba = /\/lib\.so at (0x[0-9a-f]+)/.exec(err)?.[1];
  t.check('the loader maps lib.so and libcall.so', ba && /libcall\.so at 0x/.test(err), err.trim().split('\n').slice(-5).join(' / '));
  if (!ba) return;
  await n.units({ 'libcall.so': `${d}/libcall.so` }, cache, async () => err);
  await n.units({ '/lib.so': `${d}/liba.so` }, cache, async () => err);
  const r = await run();
  const placed = count(r.stderr, /function tier for .*\/lib(call)?\.so/);
  t.eq('output as native', r.guest, native_, r.signal === 'SIGKILL' ? 'hung' : r.stderr.split('\n').filter((l) => /trap|rror/.test(l)).slice(0, 5).join(' / '));
  t.check(`exits 0, units placed ${placed} times (at least 2), same-base, same-address`, r.code === 0 && placed >= 2 && /same-base, f same-address/.test(r.stdout), `exit ${r.code}`);
  n.clean();
}
