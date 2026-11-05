// Session snapshot and restore in the browser runtime (run-node.mjs, the
// shim and glue a page loads, under Node), for test/snapshot/snapshot and
// restores. Each program (fixtures/snapshot), built with the
// snapshot flavour's sysroot (GEX_TEST_SNAP_SYSROOT: build32.sh with
// GEX_SNAPSHOT=1), is run whole, then run to its "parked" line,
// snapshotted (GEX_SNAP_OUT) and restored (GEX_SNAP_IN): the lines before
// and after the snapshot together must be the whole run's (sorted: threads'
// lines race).
import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { guest, tail, ROOT } from '../lib/guest.mjs';

export const FIX = join(ROOT, 'test/fixtures/snapshot');
const sorted = (text) => text.split('\n').filter((l) => l !== '').sort().join('\n');

export function snap(t) {
  const g = guest(t);
  const seed = g.pub('seed.x86.shared.wasm');
  const sysroot = process.env.GEX_TEST_SNAP_SYSROOT || t.skip('GEX_TEST_SNAP_SYSROOT: a snapshot-flavour sysroot (toolchain/sysroot/build32.sh with GEX_SNAPSHOT=1)');
  if (!existsSync(join(sysroot, 'lib/gex-snapshot'))) throw new Error(`${sysroot} is not a snapshot-flavour sysroot (no lib/gex-snapshot)`);
  const wcc = g.need(join(ROOT, 'wine/wcc'), 'wine/wcc');
  const dir = join(t.workDir, 'snapshot', String(process.pid));
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(dir, { recursive: true });
  const env = { ...process.env, GEX_WASM_SYSROOT: sysroot };
  const s = {
    g,
    seed,
    dir,
    /** wine/wcc with `args`, against the snapshot sysroot. */
    wcc: (args, cwd = dir) => execFileSync(wcc, args, { cwd, env, stdio: ['ignore', 'ignore', 'pipe'] }),
    /** One run of `prog` on the page's runtime: `o` g.run's, `node` env on top. */
    run: (prog, args, o, node = {}, timeout = 120000) => g.run(seed, prog, args, { ...o, env: { GEX_LOG: 'off', GEX_SHARED: '1', ...o.env, ...node }, timeout }),
    /**
     * The case NAME: whole, to "parked" and snapshotted (`delay` ms after
     * the line), `gap` s, then restored, each run with `o` (g.run's). Checks
     * each; resolves the snapshot's directory and the runs' outputs, or null
     * where a step failed.
     */
    async case(name, prog, args, o, { delay = 200, gap = 0, timeout = 120000 } = {}) {
      const whole = await s.run(prog, args, o, {}, timeout);
      if (!t.check(`${name}: whole run`, whole.code === 0, `exit ${whole.code}: ${tail(whole.stderr)}`)) return null;
      const out = join(dir, `${name}.snap`);
      rmSync(out, { recursive: true, force: true });
      const before = await s.run(prog, args, o, { GEX_SNAP_OUT: out, GEX_SNAP_AT: 'parked', GEX_SNAP_DELAY_MS: String(delay) }, timeout);
      if (!t.check(`${name}: snapshot`, before.code === 0 && existsSync(join(out, 'meta.json')), `exit ${before.code}: ${tail(before.stderr)}`)) return null;
      // time saved, which the restored clocks must not count
      if (gap) await new Promise((r) => setTimeout(r, gap * 1000));
      const after = await s.run(prog, args, o, { GEX_SNAP_IN: out }, timeout);
      t.check(`${name}: restore`, after.code === 0 && sorted(before.stdout + after.stdout) === sorted(whole.stdout),
        `exit ${after.code}; want ${tail(sorted(whole.stdout), 6)}; got ${tail(sorted(before.stdout + after.stdout), 6)}; ${tail(after.stderr, 8)}`);
      return { snap: out, whole, before, after };
    },
    /** snaptest.c, as the page runs it (a wasm32 program of wcc's). */
    snaptest() {
      const exe = join(dir, 'snaptest');
      s.wcc(['-O2', '-o', exe, join(FIX, 'snaptest.c')]);
      return { exe, o: { files: { '/t/snaptest': exe }, argv0: './snaptest', flags: ['--cwd', '/t'] } };
    },
    sorted,
  };
  return s;
}
