// The DLLs a 64-bit Wine prefix pins load at their pins in every process
// (fixtures/wine64/dllband.c): the prefix's dll-bases from
// wine/dll-bases.py, which must pass its own check, and four of the
// install's DLLs loaded at the addresses it gives them, in [0x11000000,
// 0x20000000), in the program and in a second process it starts; a
// program's own DLL in a directory whose name has a space (C:\pin dir), its
// pin a unix path with a space in it; and a fixed-base one there (no dynamic
// base) whose own base the program holds, relocated to its pin. Natively
// (no cgen) and on the page's shim, without the door; in neither may a
// driver fail to load. Also that the prefix carries 32-bit Wine's session
// and 64-bit Wine's after it (wine/dist.sh applies session.reg, then
// wine64-session.reg): the Event Log on demand, no root bus devices
// (winebus's root device on demand, where the install has game
// controllers), and MountMgr at boot (64-bit Wine's own: its drive links).
import { copyFileSync, mkdirSync, readFileSync, realpathSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { ROOT, run, wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1500 };

const BASES = join(ROOT, 'wine/dll-bases.py');
const F = join(ROOT, 'test/fixtures/wine64');

/** Why `systemReg` is not 32-bit Wine's session with 64-bit Wine's after it, or ''. */
function session(systemReg) {
  const t = `${readFileSync(systemReg, 'utf8')}\n\n`;
  const key = (svc) => new RegExp(`^\\[System\\\\\\\\ControlSet001\\\\\\\\Services\\\\\\\\${svc}\\] .*?\\n\\n`, 'msi').exec(t)?.[0] ?? '';
  const start = (svc) => { const m = /^"Start"=dword:([0-9a-f]{8})$/m.exec(key(svc)); return m && parseInt(m[1], 16); };
  const bad = [['Eventlog', 3], ['MountMgr', 2]].filter(([s, n]) => start(s) !== n).map(([s]) => `${s} Start=${start(s)}`);
  const demand = (svc) => /^"DemandStart"=dword:00000001$/m.test(key(svc));
  for (const d of ['WINEBUS', 'WINEUSB', 'WINEBTH'])
    if (!(d === 'WINEBUS' && demand('winebus')) && new RegExp(`^\\[System\\\\\\\\ControlSet001\\\\\\\\Enum\\\\\\\\ROOT\\\\\\\\WINE\\\\\\\\${d}\\]`, 'mi').test(t)) bad.push(d);
  return bad.join('; ');
}

export default async function (t) {
  const w = wine64(t, { door: 0 });
  const why = session(join(w.prefix, 'system.reg'));
  t.check('the prefix carries 32-bit Wine\'s session and 64-bit Wine\'s', !why, `${why} (wine/dist.sh's WINE64_PREFIX)`);
  const exe = await w.mingw('dllband', { src: join(F, 'dllband.c'), flags: ['-Wall'] });
  const src = (name, body) => { const f = join(w.work, `${name}.c`); writeFileSync(f, body); return f; };
  // (relocatable, as a DLL that can be pinned must be)
  const pinme = await w.mingw('pinme', { src: src('pinme', 'int pinme(void) { return 7; }\n'), flags: ['-shared', '-Wl,--dynamicbase'], out: 'pinme.dll' });
  // no dynamic base, relocations kept; its own base, 0x30000000, the program
  // holds before loading it (dllband.c FIXED_BASE)
  const fixed = await w.mingw('fixed', { src: src('fixed', 'int fixedbase(void) { return 9; }\n'),
    flags: ['-shared', '-Wl,--disable-dynamicbase,--enable-reloc-section,--image-base,0x30000000'], out: 'fixed.dll' });
  const py = w.tool('python3');
  // the prefix's dll-bases, checked; `named` is the prefix as the server
  // names its files (links resolved: the drive, not dosdevices/c:)
  const assign = async (p, at, named, map = []) => {
    mkdirSync(join(p, 'drive_c/pin dir'));
    copyFileSync(pinme, join(p, 'drive_c/pin dir/pinme.dll'));
    copyFileSync(fixed, join(p, 'drive_c/pin dir/fixed.dll'));
    const paths = [...['version', 'winmm', 'dbghelp', 'opengl32'].map((d) => `${at}/lib/wine/x86_64-windows/${d}.dll`),
      `${named}/drive_c/pin dir/pinme.dll`, `${named}/drive_c/pin dir/fixed.dll`];
    const out = join(p, 'dll-bases');
    const a = await run([py, BASES, 'assign', out, ...map], { input: `${paths.join('\n')}\n`, file: join(w.work, 'assign.log') });
    const c = a.code === 0 ? await run([py, BASES, 'check', out], { file: join(w.work, 'check.log') }) : a;
    if (!t.check('dll-bases.py assign and check', c.code === 0, c.out.trim().split('\n').slice(-3).join(' / '))) throw new Error('dll-bases.py');
  };
  const verdict = (leg, r) => {
    for (const l of r.out.split('\n').filter((l) => /^(ok  |FAIL)/.test(l))) t.say(`${leg}: ${l.trim()}`);
    t.check(`${leg}: dllband ok`, /^dllband ok/m.test(r.out), `${/still runs/.test(r.out) ? 'the child hung; ' : ''}${w.detail(r)}`);
    const drv = r.out.split('\n').filter((l) => /err:ntoskrnl:ZwLoadDriver/.test(l));
    t.check(`${leg}: no driver failed to load`, !drv.length, drv.slice(0, 4).join(' / '));
  };
  const npfx = join(realpathSync(w.work), 'native-pfx');
  verdict('native', await w.native(['C:\\dllband.exe', `Z:${npfx}/dll-bases`], {
    files: { 'dllband.exe': exe }, flags: ['--no-cgen'], timeout: 600000,
    setup: (p) => assign(p, realpathSync(w.dist), npfx),
  }));
  verdict('page', await w.page(['C:\\dllband.exe', 'Z:/pfx/dll-bases'], {
    pfxFiles: { 'dllband.exe': exe }, cwd: '/pfx/drive_c', timeout: 600000,
    setup: (p) => assign(p, '/wine64', '/pfx', ['--map', `/wine64=${w.dist}`, '--map', `/pfx=${p}`]),
  }));
  w.clean();
}
