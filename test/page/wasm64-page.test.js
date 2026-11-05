// 64-bit Wine's LP64 programs in the page's runtime, as test/native/wasm64
// runs them under gexc: each built with wine/wcc64 (wasm64 lowered to wasm32,
// ELF64 container; GEX_WASM64_SYSROOT) and run on the x86 shared shim and seed
// in the default (SharedArrayBuffer) build and the single-thread one, which
// take the same binaries; output and exit status must match the host's. Then
// --gex-guest-x86-64 (header flag 2): Windows' address space (winspace.c), and
// past 2 GiB, the NT layer's split rather than 32-bit Wine's compact one
// (winspace-high.c).
import { join } from 'node:path';
import { exec, native, tail, ROOT } from '../lib/native.mjs';
import { pool } from '../lib/gexcc.mjs';
import { lines, pageRun } from './wasmpage.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1800 };

const SOURCES = ['wasm64/lp64', 'wasm32/signals', 'wasm32/threads', 'wasm32/dlopen', 'wasm32/sjlj', 'wasm32/inet'];
const SPACES = {
  winspace: 'anonymous mapping below the USD: 1\nUSD and the page after it free: 1',
  'winspace-high': 'mapped 2560 MiB, above 2 GiB 1',
};

export default async function (t) {
  const sysroot = process.env.GEX_WASM64_SYSROOT || t.skip('GEX_WASM64_SYSROOT (toolchain/sysroot/build64.sh)');
  const n = native(t);
  const wcc64 = n.g.need(join(ROOT, 'wine/wcc64'), 'wine/wcc64');
  const seed = n.g.pub('seed.x86.shared.wasm');
  const d = n.dir('wasm64-page');
  const env = { ...n.env, GEX_WASM64_SYSROOT: sysroot };
  const build = async (name, src, flags = []) => {
    const b = await exec([wcc64, '-O2', ...flags, '-o', join(d, name), src], { cwd: d, env });
    return t.check(`${name}: builds with wcc64`, b.code === 0, tail(b.stderr, 3));
  };
  const legs = [['mp', false], ['sp', true]];
  await pool(SOURCES, async (path) => {
    const name = path.split('/')[1];
    const src = n.src(`${path}.c`);
    await n.hostcc(['-O1', '-o', join(d, `${name}.native`), src, '-lpthread']);
    if (!await build(name, src)) return;
    const h = await exec([`./${name}.native`], { cwd: d, env: n.env });
    for (const [leg, single] of legs) {
      const p = await pageRun(n.g, seed, join(d, name), { single, env: { GEX_LOG: 'off' } });
      t.check(`${name} (${leg}): as natively (exit ${h.code})`, p.out === lines(h.stdout) && p.code === h.code,
        `native exit ${h.code}, page exit ${p.code}; ${lines(h.stdout).split('\n').find((l, i) => l !== p.out.split('\n')[i]) ?? ''}; ${tail(p.stderr, 3)}`);
    }
  });
  for (const [name, want] of Object.entries(SPACES)) {
    if (!await build(name, n.src(`wasm64/${name}.c`), ['-Wl,--gex-guest-x86-64'])) continue;
    for (const [leg, single] of legs) {
      const p = await pageRun(n.g, seed, join(d, name), { single, env: { GEX_LOG: 'off' } });
      t.eq(`${name} (${leg})`, p.out, want, tail(p.stderr, 3));
    }
  }
  n.clean();
}
