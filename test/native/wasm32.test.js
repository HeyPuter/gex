// wasm32 programs checked against native execution (fixtures/native/wasm32):
// each is built for the host with cc and for gex's wasm32 with wine/wcc
// (GEX_WASM_SYSROOT: toolchain/sysroot/build32.sh's), and its output and exit
// status under gexc must match the host's. argv[0] differs; the programs
// print nothing that depends on it.
import { readdirSync } from 'node:fs';
import { join } from 'node:path';
import { exec, native, ROOT } from '../lib/native.mjs';

export const meta = { hosts: ['node'], timeout: 1800 };

export default async function (t) {
  const sysroot = process.env.GEX_WASM_SYSROOT || t.skip('GEX_WASM_SYSROOT (toolchain/sysroot/build32.sh)');
  const n = native(t);
  const wcc = n.g.need(join(ROOT, 'wine/wcc'), 'wine/wcc');
  const d = n.dir('wasm32');
  for (const f of readdirSync(n.src('wasm32')).filter((f) => f.endsWith('.c')).sort()) {
    const name = f.slice(0, -2);
    const src = n.src(`wasm32/${f}`);
    await n.hostcc(['-O1', '-o', join(d, `${name}.native`), src, '-lpthread']);
    const b = await exec([wcc, '-O2', '-o', join(d, name), src], { cwd: d, env: { ...n.env, GEX_WASM_SYSROOT: sysroot } });
    if (!t.check(`${name}: builds with wcc`, b.code === 0, b.stderr.trim().split('\n').slice(-3).join(' / '))) continue;
    const h = await exec([`./${name}.native`], { cwd: d, env: n.env });
    const g = await n.gexc(['run', '--cwd', d, `./${name}`], { cwd: d });
    t.check(`${name}: as natively (exit ${h.code})`, g.stdout === h.stdout && g.code === h.code, `native exit ${h.code}, gex exit ${g.code}; ${h.stdout.split('\n').find((l, i) => l !== g.stdout.split('\n')[i]) ?? ''}`);
  }
  n.clean();
}
