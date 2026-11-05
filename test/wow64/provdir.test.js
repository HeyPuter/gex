// Providers from outside gex (fixtures/wow64/provdir): an export provider
// from a directory (GEX_PROVIDER_DIRS: abs_sentinel.c answers msvcr120's abs
// with 4242) and a stateful provider replacing a program's own DLL
// (sdemo.dll's sd_add answers a + b + 1000, one more each call; natively
// GEX_WASM_DLLS, on the page GEX_STATEFUL, a module under the unit cache).
// Without them the library's own code answers (abs 7, sd_add 5). With a
// native i386 side msvcr120 is itself a native module, which no export
// provider replaces (abs 7). Natively (gexc run) and on the page's shim.
import { mkdirSync, writeFileSync, copyFileSync } from 'node:fs';
import { join } from 'node:path';
import { FIXTURES, ROOT, run, wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 2400 };

const SRC = join(FIXTURES, 'provdir');

export default async function (t) {
  const w = wow64(t);
  const dirs = { GEX_PROVIDER_DIRS: join(SRC, 'providers') };
  const none = { GEX_PROVIDER_DIRS: '' };
  const exe = await w.mingw('provdir', { src: join(SRC, 'provdir.c') });
  const dll = await w.mingw('sdemo', { src: join(SRC, 'sdemo.c'), flags: ['-shared'], out: 'sdemo.dll' });
  // sdemo's stateful provider, a wasm32 module against this gexc's ABI header
  const gen = join(w.work, 'gen');
  const units = join(w.work, 'units');
  mkdirSync(gen);
  mkdirSync(join(units, 'stateful'), { recursive: true });
  writeFileSync(join(gen, 'gex_abi.h'), await w.gexcRun(['cgen-c', '--header'], { log: 'header' }));
  copyFileSync(join(ROOT, 'crates/gex-cgen/providers/provider.h'), join(gen, 'provider.h'));
  const obj = join(w.work, 'sdemo_prov.o');
  const prov = join(units, 'stateful/sdemo.wasm');
  for (const argv of [
    [w.tool('clang'), '--target=wasm32', '-O2', '-fPIC', '-fvisibility=hidden', '-matomics', '-mbulk-memory', `-I${gen}`, '-c', join(SRC, 'sdemo_prov.c'), '-o', obj],
    [w.tool('wasm-ld'), '--experimental-pic', '-shared', '--no-entry', '--import-memory', '--shared-memory', '--max-memory=4294901760',
      '--allow-undefined', '-o', prov, obj],
  ]) {
    const r = await run(argv, { file: join(w.work, 'prov.log') });
    if (r.code !== 0) throw new Error(`sdemo's provider: ${r.out.trim().split('\n').slice(-3).join(' / ')}`);
  }
  // (the provider modules with GEX_PROVIDER_DIRS: the directory's own among them)
  await w.gexcRun(['cgen-image', '--providers'], { env: { GEX_CGEN_CACHE: units, ...dirs }, log: 'providers' });
  const pabs = w.i386Native ? 7 : 4242;
  const verdict = (leg, r, abs, add1, add2) => {
    const got = r.out.split('\n').map((l) => l.replace(/\r$/, '')).filter((l) => /^(abs|sd_add) /.test(l)).join(' ');
    const want = `abs ${abs} sd_add ${add1} sd_add ${add2}`;
    t.check(`${leg}: ${want}`, /^provdir done/m.test(r.out) && got === want, `got '${got}'; ${w.detail(r, 6)}`);
  };
  const base = (r) => { const m = /^msvcr120 at (?:0x)?([0-9a-fA-F]+)/m.exec(r.out); return m ? `0x${m[1]}` : null; };
  const build = (b) => w.unit(join(w.dist, 'lib/wine/i386-windows/msvcr120.dll'), units, { base: b, modes: [{ GEX_CGEN_PRECOMPILE: '1', ...dirs }] });
  const native = (more, name) => w.native(['C:\\provdir.exe'], { files: { 'provdir.exe': exe, 'sdemo.dll': dll },
    env: { GEX_CGEN_CACHE: units, ...dirs, ...more }, name });
  const page = (more, name) => w.page(['/t/provdir.exe'], { files: { '/t/provdir.exe': exe, '/t/sdemo.dll': dll }, flags: ['--cgen-prefer'],
    env: { GEX_CGEN_CACHE: units, ...dirs, ...more }, name });

  const n0 = await native(none, 'n0');
  verdict('native, no providers', n0, 7, 5, 5);
  const nbase = base(n0);
  if (nbase) await build(nbase);
  verdict('native, providers from outside gex', await native({ GEX_WASM_DLLS: `sdemo.dll=${prov}` }, 'n1'), pabs, 1005, 1006);
  const stateful = { GEX_STATEFUL: 'sdemo.dll=stateful/sdemo.wasm' };
  let p1 = await page(stateful, 'p1');
  const pbase = base(p1);
  if (pbase && pbase !== nbase) {
    await build(pbase);
    p1 = await page(stateful, 'p1b');
  }
  verdict('page, providers from outside gex', p1, pabs, 1005, 1006);
  verdict('page, no providers', await page(none, 'p0'), 7, 5, 5);
  w.clean();
}
