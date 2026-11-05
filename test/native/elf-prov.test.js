// ELF export providers against translated glibc: each program
// (fixtures/native/elf-prov, x86-64 C against the host's glibc) runs under
// gexc with GEX_PROVIDERS=0 and with providers on; the outputs and exit
// statuses must be identical, the run must exit 0, and with providers on the
// libraries' exports must actually have been bound (GEX_PROVIDER_TRACE).
import { readdirSync } from 'node:fs';
import { join } from 'node:path';
import { native, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1800 };

export default async function (t) {
  const n = native(t);
  const d = n.dir('elf-prov');
  for (const f of readdirSync(n.src('elf-prov')).filter((f) => f.endsWith('.c')).sort()) {
    const name = f.slice(0, -2);
    const exe = join(d, name);
    await n.hostcc(['-O1', '-fno-builtin', '-o', exe, n.src(`elf-prov/${f}`), '-lm', '-pthread']);
    const [off, on] = await Promise.all(['0', '1'].map((pv) => n.gexc(['run', exe], { env: { GEX_PROVIDERS: pv, GEX_PROVIDER_TRACE: '1' }, timeout: 600000 })));
    const bound = on.stderr.split('\n').map((l) => /^gex: provider .*: (\d+) addresses bound/.exec(l)?.[1]).filter(Boolean).map(Number);
    if (!t.check(`${name}: a provider bound`, bound.length > 0, tail(on.stderr))) continue;
    t.check(`${name}: exits 0 without providers`, off.code === 0, `exit ${off.code}: ${tail(off.stderr)}`);
    const lines = off.stdout.split('\n').length;
    t.check(`${name}: with providers as without (${lines} lines, ${bound.reduce((a, b) => a + b, 0)} addresses bound)`, on.stdout === off.stdout && on.code === off.code,
      `exit ${on.code} vs ${off.code}; ${tail(on.stderr)}`);
  }
  n.clean();
}
