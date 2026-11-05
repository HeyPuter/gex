// 64-bit Wine's data model: its PE side for x64 is compiled for wasm64 with
// the GexLLP64 plugin, and every structure it shares with x64 programs must
// have the layout x86-64 Windows code expects. fixtures/wine64/layout.c
// names the Win32, NT and wineserver types; clang's record layouts (every
// nested record, sizes, alignments, field offsets) dumped for
// wasm64+GexLLP64 and for x86_64-w64-mingw32 (Wine's own x64 PE target,
// MSVC bitfields) must be identical. Needs the Wine tree
// (GEX_TEST_WINE64_SRC) and clang; the plugin is built into the test's
// work directory (toolchain/sysroot/llp64/build.sh).
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { execFileSync } from 'node:child_process';
import { ROOT, run } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 600 };

export default async function (t) {
  const wine = process.env.GEX_TEST_WINE64_SRC || t.skip('GEX_TEST_WINE64_SRC: the 64-bit Wine tree');
  if (!readFileSync(join(wine, 'include/basetsd.h'), 'utf8').includes('__GEX_LLP64__')) throw new Error(`${wine} has no 64-bit Wine headers`);
  const work = join(t.workDir, 'wine64-layout', String(process.pid));
  rmSync(work, { recursive: true, force: true });
  mkdirSync(work, { recursive: true });
  const b = await run(['sh', join(ROOT, 'toolchain/sysroot/llp64/build.sh'), work], { file: join(work, 'build.log'), timeout: 300000 });
  if (!t.check('the LLP64 plugin builds', b.code === 0 && existsSync(join(work, 'GexLLP64.so')), b.out.trim().split('\n').slice(-3).join(' / '))) return;
  const res = join(execFileSync('clang', ['-print-resource-dir'], { encoding: 'utf8' }).trim(), 'include');
  const common = ['-mms-bitfields', '-fshort-wchar', '-nostdinc', `-I${wine}/include`, `-I${wine}/include/msvcrt`, '-isystem', res, '-D_WIN32', '-D__WINESRC__',
    '-fsyntax-only', '-w', '-Xclang', '-fdump-record-layouts', join(ROOT, 'test/fixtures/wine64/layout.c')];
  const dump = (args) => execFileSync('clang', args, { encoding: 'utf8', maxBuffer: 1 << 30 });
  const x64 = dump(['--target=x86_64-w64-mingw32', ...common]);
  const w64 = dump(['--target=wasm64-unknown-unknown', `-fplugin=${join(work, 'GexLLP64.so')}`, '-include', join(work, 'llp64.h'), '-D_WIN64', ...common]);
  const n = x64.split('Dumping AST Record Layout').length - 1;
  t.check(`${n} records dumped (at least 1000)`, n >= 1000);
  let why = '';
  if (x64 !== w64) {
    writeFileSync(join(work, 'x86_64.txt'), x64);
    writeFileSync(join(work, 'wasm64.txt'), w64);
    const a = x64.split('\n'), c = w64.split('\n');
    const i = a.findIndex((l, k) => l !== c[k]);
    why = `first difference at line ${i + 1}: x86_64 ${JSON.stringify(a[i])}, wasm64 ${JSON.stringify(c[i])} (both in ${work})`;
  }
  t.check('wasm64 (GexLLP64) layouts identical to x86_64', !why, why);
  if (!why) rmSync(work, { recursive: true, force: true });
}
