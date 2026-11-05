// wasm32 programs in the page's runtime (fixtures/native/wasm32, as
// test/native/wasm32 runs them under gexc): each built for the host with cc
// and with wine/wcc (GEX_WASM_SYSROOT), run on this arch's shared shim and
// seed; its output and exit status must match the host's. With BUSYBOX (a
// wasm32 busybox), fixtures/native/busybox-script.sh under it as the page's
// /bin/sh (run-node.mjs --busybox) against the host's sh.
import { existsSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { exec, native, tail, ROOT } from '../lib/native.mjs';
import { pool } from '../lib/gexcc.mjs';
import { lines, pageRun } from './wasmpage.mjs';

export const meta = { hosts: ['node'], timeout: 1800 };

export default async function (t) {
  const sysroot = process.env.GEX_WASM_SYSROOT || t.skip('GEX_WASM_SYSROOT (toolchain/sysroot/build32.sh)');
  const n = native(t);
  const wcc = n.g.need(join(ROOT, 'wine/wcc'), 'wine/wcc');
  const seed = n.seed();
  const d = n.dir('wasm32-page');
  const names = readdirSync(n.src('wasm32')).filter((f) => f.endsWith('.c')).sort().map((f) => f.slice(0, -2));
  await pool(names, async (name) => {
    const src = n.src(`wasm32/${name}.c`);
    await n.hostcc(['-O1', '-o', join(d, `${name}.native`), src, '-lpthread']);
    const b = await exec([wcc, '-O2', '-o', join(d, name), src], { cwd: d, env: { ...n.env, GEX_WASM_SYSROOT: sysroot } });
    if (!t.check(`${name}: builds with wcc`, b.code === 0, tail(b.stderr, 3))) return;
    const h = await exec([`./${name}.native`], { cwd: d, env: n.env });
    const p = await pageRun(n.g, seed, join(d, name), { env: { GEX_LOG: 'off' } });
    t.check(`${name}: as natively (exit ${h.code})`, p.out === lines(h.stdout) && p.code === h.code,
      `native exit ${h.code}, page exit ${p.code}; ${lines(h.stdout).split('\n').find((l, i) => l !== p.out.split('\n')[i]) ?? ''}; ${tail(p.stderr, 3)}`);
  });
  const bb = process.env.BUSYBOX;
  if (bb && existsSync(bb)) {
    const script = readFileSync(n.src('busybox-script.sh'), 'utf8');
    writeFileSync(join(d, 'in.txt'), 'b\na\nc\nb\n');
    const h = await exec(['sh', '-c', script], { cwd: d, env: n.env });
    const r = await n.g.run(seed, bb, ['-c', script], { files: { '/t/in.txt': join(d, 'in.txt') }, argv0: 'sh', flags: ['--busybox', bb, '--cwd', '/t'],
      env: { GEX_SHARED: '1', GEX_LOG: 'off' }, merge: true, timeout: 300000 });
    t.eq('busybox as /bin/sh', r.guest, lines(h.stdout + h.stderr));
  } else t.say('busybox: BUSYBOX not set, not run');
  n.clean();
}
