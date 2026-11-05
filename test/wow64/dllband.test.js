// The DLLs a WoW64 prefix pins load in the band (dllband.c): the prefix's
// dll-bases from wine/dll-bases.py (its 64-bit builtins, dll-pins.txt,
// first, then the 32-bit DLLs with --i386), which must pass its own check,
// and four of the dist's i386 DLLs loaded at the addresses it gives them, in
// [0x11000000, 0x20000000), and a program's own DLL in a directory whose
// name has a space (C:\pin dir), its pin a unix path with a space in it.
// The server names an image by the unix path it opened: natively the
// dist's and the prefix's, on the page their page paths. Natively (gexc
// run, a Wayland compositor of its own) and on the page's shim, in the
// compact space only (the split space is 32-bit Wine's).
import { mkdirSync, copyFileSync, readFileSync, realpathSync } from 'node:fs';
import { join } from 'node:path';
import { FIXTURES, ROOT, run, wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 900 };

const BASES = join(ROOT, 'wine/dll-bases.py');

export default async function (t) {
  const w = wow64(t);
  const exe = await w.mingw('dllband', { flags: ['-Wall'] });
  const pinme = await w.mingw('pinme', { src: join(FIXTURES, 'dllband-pinme.c'), flags: ['-shared', '-Wl,--dynamicbase'], out: 'pinme.dll' });
  const py = w.tool('python3');
  const pins = readFileSync(join(ROOT, 'wine/dll-pins.txt'), 'utf8').split('\n').filter((l) => l && !l.startsWith('#'));
  // the prefix's dll-bases for distPath's and pfxPath's DLLs (`map`: where
  // those are on this machine)
  const assign = async (p, distPath, pfxPath, map = []) => {
    mkdirSync(join(p, 'drive_c/pin dir'));
    copyFileSync(pinme, join(p, 'drive_c/pin dir/pinme.dll'));
    const out = join(p, 'dll-bases');
    const steps = [
      [pins.map((l) => `${distPath}/lib/wine/x86_64-windows/${l}`), []],
      [[...['version', 'winmm', 'dbghelp', 'opengl32'].map((x) => `${distPath}/lib/wine/i386-windows/${x}.dll`), `${pfxPath}/drive_c/pin dir/pinme.dll`], ['--i386']],
    ];
    for (const [paths, more] of steps) {
      const r = await run([py, BASES, 'assign', out, ...more, ...map], { input: paths.join('\n') + '\n', file: join(w.work, 'assign.log') });
      if (r.code !== 0) throw new Error(`dll-bases.py assign: ${r.out.trim().split('\n').slice(-3).join(' / ')}`);
    }
    return out;
  };
  const verdict = (leg, r) => {
    for (const l of r.out.split('\n').filter((l) => /^(ok  |FAIL)/.test(l))) t.say(`${leg}: ${l}`);
    t.check(`${leg}: dllband ok`, /^dllband ok/m.test(r.out), w.detail(r));
  };
  // (natively the prefix's real path, links resolved, as the server names it)
  const npfx = join(realpathSync(w.work), 'native-pfx');
  verdict('native', await w.native(['./dllband.exe', `Z:${npfx}/dll-bases`], {
    files: { 'dllband.exe': exe }, flags: ['--no-cgen'], wayland: true, timeout: 200000,
    setup: async (p) => {
      const out = await assign(p, realpathSync(w.dist), npfx);
      const c = await run([py, BASES, 'check', out], { file: join(w.work, 'check.log') });
      if (!t.check('dll-bases.py check', c.code === 0, c.out.trim().split('\n').slice(-3).join(' / '))) throw new Error('dll-bases.py check');
    },
  }));
  verdict('page compact', await w.page(['/t/dllband.exe', 'Z:/pfx/dll-bases'], {
    files: { '/t/dllband.exe': exe }, env: { GEX_DEBUG_EXIT: '1' },
    setup: (p) => assign(p, '/wine32', '/pfx', ['--map', `/wine32=${w.dist}`, '--map', `/pfx=${p}`]),
  }));
  w.clean();
}

