// ELF export providers in the page's runtime, as test/native/elf-prov checks
// them under gexc: each program (fixtures/native/elf-prov, x86-64 C against
// the host's glibc, whose ld.so and libc the guest gets) runs with
// GEX_PROVIDERS=0 and with providers on; the outputs must match each other
// and gexc's, the run must exit 0, and with providers on (and only then) the
// libraries' exports must have been bound. The provider modules come from the
// unit cache a native run fills (GEX_CGEN_CACHE, the test's own).
import { existsSync, readdirSync, realpathSync } from 'node:fs';
import { join } from 'node:path';
import { native, tail } from '../lib/native.mjs';
import { pool } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 2400 };

const LIBS = ['/lib64/ld-linux-x86-64.so.2', '/lib/x86_64-linux-gnu/libc.so.6', '/lib/x86_64-linux-gnu/libm.so.6'];
const bound = (err) => err.split('\n').filter((l) => /addresses bound/.test(l)).length;

export default async function (t) {
  for (const l of LIBS) if (!existsSync(l)) t.skip(`no ${l} on this host`);
  const n = native(t);
  n.g.pub('gex_browser.x86.shared.wasm');
  const d = n.dir('elf-prov-page');
  const cache = join(d, 'cgen');
  const files = Object.fromEntries(LIBS.map((l) => [l, realpathSync(l)]));
  const names = readdirSync(n.src('elf-prov')).filter((f) => f.endsWith('.c')).sort().map((f) => f.slice(0, -2));
  await pool(names, async (name) => {
    const exe = join(d, name);
    await n.hostcc(['-O1', '-fno-builtin', '-o', exe, n.src(`elf-prov/${name}.c`), '-lm', '-pthread']);
    const nat = await n.gexc(['run', exe], { env: { GEX_CGEN_CACHE: cache }, timeout: 600000 });
    if (!t.check(`${name}: runs under gexc`, nat.code === 0, `exit ${nat.code}: ${tail(nat.stderr)}`)) return;
    const b = await n.gexc(['build', exe, '-o', `${exe}.wasm`]);
    if (!t.check(`${name}: gexc build`, b.code === 0, tail(b.stdout + b.stderr))) return;
    const [off, on] = await Promise.all(['0', '1'].map((pv) => n.g.run(`${exe}.wasm`, exe, [], {
      files: { [exe]: exe, ...files }, flags: ['--cgen-prefer'], cwd: d, timeout: 900000,
      env: { GEX_PROVIDERS: pv, GEX_SHARED: '1', GEX_LOG: 'debug', GEX_CGEN_CACHE: cache },
    })));
    const strip = (out) => out.split('\n').filter((l) => !l.startsWith('[')).join('\n');
    t.check(`${name}: bound with providers only (${bound(on.stderr)} times; without ${bound(off.stderr)})`, bound(on.stderr) > 0 && bound(off.stderr) === 0, tail(on.stderr));
    t.check(`${name}: exits 0 without providers`, off.code === 0, `exit ${off.code}: ${tail(off.stderr)}`);
    t.check(`${name}: with providers as without, and as gexc's run`, strip(on.stdout) === strip(off.stdout) && on.code === off.code && strip(on.stdout) === nat.stdout,
      `exit ${on.code} vs ${off.code}; ${tail(on.stderr)}`);
  });
  n.clean();
}
