// A struct by value from native wasm32 code into the i386 side's DLLs
// (fixtures/wow64/ptval32.c), in a WoW64 process whose i386 side is native
// wasm32: the program is a Wine builtin of the i386 wasm32 tree linked into
// the install's i386 core (pelink --extra, the core's exports kept), so its
// calls of oleaut32, shlwapi and comctl32, modules of their own, go through
// pelink's import stubs; i386 takes the struct's bytes on the stack, wasm32
// hands the stub a pointer to it. Natively and on the page: "0 failures".
// Needs the install's i386 wasm32 tree and its sysroot (wine/dist.sh's
// WINE64_BUILD32 and WINE64_SYSROOT32).
import { copyFileSync, mkdirSync, readdirSync, readFileSync, rmSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { join } from 'node:path';
import { graphics, wow64, run, FIXTURES, LOADER, ROOT } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1200 };

const IMPORTS = 'ucrtbase.dll,kernel32.dll,ntdll.dll,user32.dll,oleaut32.dll,shlwapi.dll,comctl32.dll';
const LIBS = ['oleaut32', 'shlwapi', 'comctl32', 'user32', 'ucrtbase', 'kernel32', 'ntdll'];

export default async (t) => {
  const w = wow64(t);
  if (!w.i386Native) t.skip(`${w.dist}: the i386 side is translated (no i386-windows/winepe.wasm)`);
  const B = process.env.GEX_TEST_WOW64_BUILD32 || t.skip("GEX_TEST_WOW64_BUILD32: the install's i386 wasm32 tree (wine/dist.sh's WINE64_BUILD32)");
  const sysroot = process.env.GEX_TEST_WOW64_SYSROOT32 || t.skip("GEX_TEST_WOW64_SYSROOT32: its sysroot (wine/dist.sh's WINE64_SYSROOT32)");
  const mk = readFileSync(join(B, 'Makefile'), 'utf8');
  const S = mk.match(/^srcdir = (.+)$/m)?.[1] ?? t.skip(`${B}/Makefile names no srcdir`);
  const tool = (n) => mk.match(new RegExp(`\\S*/tools/${n}/${n}`))?.[0];
  const winebuild = tool('winebuild'), winegcc = tool('winegcc');
  const wcc = join(ROOT, 'wine/wcc');
  const b = join(w.work, 'b');
  mkdirSync(b, { recursive: true });
  const env = { ...process.env, GEX_WASM_SYSROOT: sysroot, TMPDIR: b };
  const step = async (what, argv, cwd = b) => {
    const r = await run(argv, { cwd, env, file: join(w.work, `${what}.log`), timeout: 600000 });
    if (r.code !== 0) throw new Error(`ptval32 ${what}: ${r.out.trim().split('\n').slice(-5).join(' / ')}`);
    return r.out;
  };

  // the program, as a Wine builtin of the i386 wasm32 tree
  await step('cc', [wcc, '-c', '-o', join(b, 'ptval32.o'), join(FIXTURES, 'ptval32.c'), `-I${B}/include`, `-I${S}/include`, `-I${S}/include/msvcrt`, '-D_UCRT',
    '-fvisibility=hidden', '-fno-strict-aliasing', '-fPIC', '-D_WIN32', '-fno-builtin', '-fshort-wchar', '-Wno-format', '-O2']);
  await step('link', [winegcc, '-o', 'ptval32.exe.so', '--wine-objdir', B, '--winebuild', winebuild, `--cc-cmd=${wcc}`, '-b', 'wasm32-unknown-linux-musl',
    '-mno-cygwin', '-fPIC', '-mconsole', 'ptval32.o', ...LIBS.map((l) => `${B}/dlls/${l}/lib${l}.a`), `${B}/libs/winecrt0/libwinecrt0.a`]);

  // the install with its i386 core relinked with the program: the core's
  // DLLs (the skeletons that name winepe.wasm), its exports kept for the
  // modules of their own beside it
  const d = join(w.work, 'dist');
  execFileSync('cp', ['-rl', w.dist, d]);
  const dd = join(d, 'lib/wine/i386-windows');
  const core = readdirSync(dd).filter((f) => /\.(dll|exe|drv)$/.test(f) && readFileSync(join(dd, f)).includes('GEXWREF1winepe.wasm\0'));
  t.check('the i386 core has its DLLs', core.includes('ntdll.dll'), core.join(' '));
  const pl = await step('pelink', ['python3', join(ROOT, 'wine/pelink.py'), '--exports-of', join(dd, 'winepe.wasm'),
    '--extra', 'ptval32.exe', join(b, 'ptval32.exe.so.r.o'), join(b, 'ptval32.exe.so.exports'), IMPORTS, B, S, join(b, 'winepe.wasm'), ...core]);
  t.check('pelink: imports with a struct by value', /with a struct by value/.test(pl), pl.split('\n').filter((l) => l.includes('imports from')).join(' / '));
  rmSync(join(dd, 'winepe.wasm'));
  copyFileSync(join(b, 'winepe.wasm'), join(dd, 'winepe.wasm'));
  await step('skeleton', [winebuild, '-w', '-b', 'wasm32-unknown-linux-musl', `--cc-cmd=${wcc}`, '--exe', '--fake-module', '--wasm-shared', 'winepe.wasm',
    '-F', 'ptval32.exe', '-o', join(dd, 'ptval32.exe')]);

  // (LBItemFromPt's listbox needs a display driver: Wine's null driver)
  const want = /^0 failures/m;
  const n = await w.native([join(dd, 'ptval32.exe')], { loader: join(d, LOADER), userReg: graphics('null'), timeout: 300000 });
  t.check('native: 0 failures', want.test(n.out), w.detail(n, 12));
  const p = await w.page(['/wine32/lib/wine/i386-windows/ptval32.exe'], { dist: d, userReg: graphics('null'), timeout: 300000 });
  t.check('page: 0 failures', want.test(p.out), w.detail(p, 12));
  w.clean();
};
