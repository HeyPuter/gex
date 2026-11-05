// The x86 programs (programs.mjs) on the page's shim: programs.mjs's PAGE,
// their x86-64 builds under run-node.mjs with the memory64 shared seed,
// against the host's own run.
import { pool } from '../lib/gexcc.mjs';
import { PAGE, env, x86Programs, differ } from './programs.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 3600 };

const TARGET = 'x86_64-linux-musl';
const SELF_EXEC = new Set(['exec-signals', 'exec-futex', 'spawn-threads', 'proc-threads', 'fs-threads', 'exec-dethread', 'fork-thread-ctx', 'file-stale-loop']);

export default async function (t) {
  const x = x86Programs(t);
  const seed = x.g.pub('seed.x86.m64.shared.wasm');
  x.g.pub('gex_browser.x86.m64.shared.wasm');
  // threads-many and many-workers take hundreds of Workers each: alone, after the rest
  const solo = new Set(['threads-many', 'many-workers']);
  const one = async (name) => {
    const leg = `${name} (page)`;
    let bin;
    try {
      bin = await x.build(name, TARGET);
    } catch (e) {
      return t.check(`${leg}: builds`, false, e.message);
    }
    const args = await x.args(name);
    const n = await x.exec([bin, ...args]);
    const opts = name === 'served-entries' ? { files: { [`/bin/${name}`]: bin, [`/bin/${name}-b`]: `${bin}-b` }, argv0: `/bin/${name}` }
      : SELF_EXEC.has(name) ? { files: { [`/bin/${name}`]: bin }, argv0: `/bin/${name}` } : {};
    const run = () => x.g.run(seed, bin, args, { ...opts, env: { GEX_LOG: 'off', GEX_SHARED: '1', GEX_MEM64: '1', ...env(name, true) }, timeout: 120000 });
    let p = await run();
    // 125: run-node's flaky "nothing could wake it" after a threaded
    // program has finished; once more
    if (p.code === 125) {
      t.say(`${leg}: exit 125, once more`);
      p = await run();
    }
    let out = p.stdout.split('\n').filter((l) => !l.startsWith('[host]')).join('\n').replace(/\n+$/, '');
    if (name === 'dense-holes' && !p.stderr.includes('dense holes reached the miss stub')) out += ' (no dense hole reached the miss stub)';
    if (name === 'fork-dirs' && p.stderr.includes('gex: heal')) out += ' (heal lines)';
    const code = p.code ?? 128;
    t.check(`${leg}: as natively (exit ${n.code})`, n.out === out && n.code === code, `native exit ${n.code}, gex exit ${code}; ${differ(n.out, out)}; ${p.stderr.trim().split('\n').slice(-3).join(' / ')}`);
  };
  // GEX_TEST_ONLY: those programs alone (a comma list), to look at one
  const only = process.env.GEX_TEST_ONLY?.split(',');
  if (only) {
    for (const n of only) await one(n);
    return x.done();
  }
  await pool([...PAGE].filter((n) => !solo.has(n)).sort(), one);
  for (const n of solo) await one(n);
  x.done();
}
