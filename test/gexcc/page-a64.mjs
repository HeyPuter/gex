// What test/gexcc's page tests on aarch64 share: a root of the guest's (the
// musl cross toolchain's loader in lib/), and a run of a program in it on the
// page's shim under Node (run-node.mjs, the shared seed).
import { copyFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { native } from '../lib/native.mjs';

export function pageA64(t) {
  const n = native(t);
  const seed = n.g.pub('seed{infix}.shared.wasm');
  const [[ld, libc]] = Object.entries(n.ldso);
  return {
    n,
    seed,
    /** A fresh directory NAME with root/lib (the loader) and root/bin. */
    root(name) {
      const d = n.dir(name);
      for (const s of ['root/lib', 'root/bin']) mkdirSync(join(d, s), { recursive: true });
      copyFileSync(libc, join(d, `root${ld}`));
      return d;
    },
    /** PROG (a path in ROOT) on the page, `flags` run-node's own, `env` node's. */
    run: (root, prog, { args = [], flags = [], env = {}, timeout = 300000 } = {}) =>
      n.g.run(seed, join(root, prog), args, { argv0: prog, flags: ['--dir', `/=${root}`, ...flags], env: { GEX_SHARED: '1', ...env }, timeout, merge: true }),
  };
}
