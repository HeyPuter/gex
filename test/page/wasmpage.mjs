// A program under run-node.mjs (the page's shim and glue under Node), as a
// page runs it: at /t/NAME, its cwd /t, argv[0] ./NAME. Resolves `{ out,
// code, stderr }`, `out` the guest's lines (the runtime's own dropped);
// `single`: the single-thread build (GEX_SINGLE=1, the SP page's).
import { basename } from 'node:path';

export async function pageRun(g, seed, prog, { single = false, env = {}, flags = [], files = {}, timeout = 300000 } = {}) {
  const name = basename(prog);
  const r = await g.run(seed, prog, [], { files: { [`/t/${name}`]: prog, ...files }, argv0: `./${name}`, flags: ['--cwd', '/t', ...flags], env: { GEX_SHARED: '1', ...env }, single, timeout });
  return { out: r.guest, code: r.code ?? 128, stderr: r.stderr };
}

/** A host run's stdout as `pageRun`'s `out` gives a guest's: no empty lines. */
export const lines = (text) => text.split('\n').filter(Boolean).join('\n');
