// Function-tier units served to the System (`new System({ units })`): the
// `wine64-units` image's, built for the test programs and Wine's core DLLs at
// the bases they load at (the core's only where the dist translates it; a
// native PE core runs it as wasm). The programs run right with them, and the
// runtime names none of those images as lacking a unit: each was used.
import { bootWine } from '../lib/wine.mjs';
import { UNIT_PROGS } from '../fixtures/wine/units.mjs';

export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['wine64', 'win-progs', 'wine64-units'], timeout: 900 };

const CASES = {
  'hello_args.exe': [['a', 'b c'], 42, /^hello from a Windows program with 3 args\narg 1: a\narg 2: b c\n$/],
  'seh.exe': [[], 77, /^(ok [^\n]*\n)+top-level filter saw e00000ff\n0 failures\n?$/],
};

export default async function (t) {
  const exe = UNIT_PROGS[t.phase - 1];
  const [args, code, out] = CASES[exe];
  const w = await bootWine(t, { system: { units: t.work('wine64-units') } });
  const r = await t.time(exe, () => w.run(exe, args)).catch((e) => ({ code: -1, stdout: '', stderr: String(e.message) }));
  t.check(`${exe}: exit ${code}, its output`, r.code === code && out.test(r.stdout), `exit ${r.code}; stdout ${JSON.stringify(r.stdout.slice(-300))}; stderr ${JSON.stringify(r.stderr.trim().split('\n').slice(-4).join(' / '))}`);
  await w.sys.shutdown();
  const re = /gex-pe: (image \S+ at base 0x[0-9a-f]+: no function-tier unit|missing unit: \[the trampolines\])/;
  const missed = [...await t.logs(re), ...w.sys.state.console.split('\n').filter((l) => re.test(l))]
    .map((l) => re.exec(l)[1].replace(': no function-tier unit', ''))
    .filter((m) => /\/(ntdll|kernelbase|kernel32|ucrtbase)\.dll |trampolines|c:\/[a-z_]+\.exe /.test(m));
  t.check(`${exe}: a unit for ntdll, kernelbase, kernel32, ucrtbase, the program and the trampolines`, !missed.length, [...new Set(missed)].join(', '));
  if (t.phase < UNIT_PROGS.length) return t.reload();
}
