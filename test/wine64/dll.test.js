// A native DLL for 64-bit Wine: a lowered wasm64 module behind a PE32+
// skeleton (fixtures/wine64/dll/build.sh), called from x64 code through
// every kind of Win64 entry adapter, and calling back, under gexc with the
// door. Beside the native PE core it is a split module against it, built
// in the install's wasm64 tree. Needs the native build of the Wine fork's
// tools (GEX_TEST_WINE64_TOOLS, wine/dist.sh's WINE64_TOOLS), its tree
// (GEX_TEST_WINE64_SRC), that wasm64 tree (GEX_TEST_WINE64_BUILD) and the
// wasm64 sysroot (GEX_WASM64_SYSROOT).
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { ROOT, run, wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1800 };

export default async function (t) {
  const env = process.env;
  for (const k of ['GEX_TEST_WINE64_TOOLS', 'GEX_TEST_WINE64_SRC', 'GEX_WASM64_SYSROOT']) if (!env[k]) t.skip(`${k} is not set`);
  const w = wine64(t);
  const out = join(w.work, 'dll');
  const build = w.nativeCore ? env.GEX_TEST_WINE64_BUILD || t.skip('GEX_TEST_WINE64_BUILD: the install\'s wasm64 tree (wine/dist.sh\'s WINE64_BUILD)') : '';
  // (in the run's directory: winebuild leaves its temporary files in the current one)
  const r = await run(['sh', join(ROOT, 'test/fixtures/wine64/dll/build.sh'), out, env.GEX_TEST_WINE64_SRC, env.GEX_TEST_WINE64_TOOLS],
    { cwd: w.work, env: { ...env, WINE64_BUILD: build }, timeout: 900000, file: join(w.work, 'dll.build') });
  if (t.check('dll: build', r.code === 0, r.out.trim().split('\n').slice(-5).join(' / '))) {
    const extra = [join(out, 'abi.dll'), ...(existsSync(join(out, 'abi.dll.wasm')) ? [join(out, 'abi.dll.wasm')] : [])];
    await w.nt({ name: 'dll', want: /(ok [^\n]*\n)+PASS/, exe: join(out, 'abi_test.exe'), extra });
  }
  w.clean();
}
