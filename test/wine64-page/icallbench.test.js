// Guest-to-native call cost (fixtures/wine64/icallbench.c against convdll.c,
// GEX_TEST_GEXCC_WIN): guest x86-64 code calling native functions through
// their table indices (the module's entry path) and an export, with
// guest-to-guest and native-to-native references, natively and on the
// page (native PE core, GEX_TEST_WINE64_NATIVE). Timed: run it alone on the
// bench host (harness/bench/bench.sh); each kind's ns per call is in the
// result's times as "LEG KIND ns".
// ICALL_BENCH_N: calls per kind (default 2000000); ICALL_BENCH_DLL below.
import { join } from 'node:path';
import { wine64, ROOT } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1800, timed: true };

export default async function (t) {
  const F = join(ROOT, 'test/fixtures/wine64');
  const w = wine64(t, { door: 1, native: true });
  // ICALL_BENCH_DLL: convdll.dll built already (its .wasm beside it), for a
  // bench host without gexcc-win
  const dll = process.env.ICALL_BENCH_DLL || await w.gexccWin(join(F, 'convdll.c'), 'convdll.dll', ['-O2', '-shared', join(F, 'convdll.def')]);
  const exe = await w.mingw('icallbench', { x64: true, src: join(F, 'icallbench.c'), flags: [`-I${F}`, '-O2'] });
  const n = process.env.ICALL_BENCH_N ?? '2000000';
  const want = /bench [^\n]*\n[^]*ok icallbench/;
  for (const leg of ['native', 'page']) {
    const r = await w.nt({ name: `icallbench ${leg}`, exe, args: [n], extra: [dll, `${dll}.wasm`], want: /[^]*ok icallbench[^]*/,
      ...(leg === 'page' && { page: {} }), timeout: 900000 });
    for (const m of (r?.stdout ?? '').matchAll(/^bench (.+) ([0-9.]+) ns\/call$/gm)) {
      t.say(`${leg}: ${m[1]} ${m[2]} ns/call`);
      t.record(`${leg} ${m[1]} ns`, Number(m[2]));
    }
    t.check(`${leg}: every kind measured`, want.test(r?.stdout ?? ''), r?.stdout?.slice(-300));
  }
  w.clean();
}
