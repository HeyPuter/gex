// A fixture of fixtures/native built with the host's cc and run there for
// the expected output, and built static for the guest (zig, the page's
// image base) and run natively (gexc run) and in the page (run-node.mjs on
// the shared shim and kernel): each run's output must be the host's.
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { exec, native, tail } from '../lib/native.mjs';

export async function hostMatch(t, fixture) {
  const n = native(t);
  const name = fixture.split('/').pop().replace(/\.c$/, '');
  const d = n.dir(name);
  const src = n.src(fixture);
  await n.hostcc(['-O1', '-w', '-o', join(d, `${name}.host`), src]);
  const prog = n.g.cc({ name, code: readFileSync(src, 'utf8') });
  n.g.pub('gex_browser{infix}.shared.wasm');
  n.g.pub('gex_kernel{infix}.wasm');
  const want = (await exec([join(d, `${name}.host`)], { cwd: d, timeout: 120000 })).stdout;
  if (!want.trim()) throw new Error(`${name} printed nothing on the host`);
  const [nat, page] = await Promise.all([
    n.gexc(['run', prog], { cwd: d, timeout: 120000 }),
    n.g.run(n.g.seed(), prog, [], { env: { GEX_SHARED: '1' }, cwd: d, timeout: 300000 }),
  ]);
  t.eq('native', nat.stdout, want, tail(nat.stderr));
  t.eq('page', page.guest, want.replace(/\n+$/, '').split('\n').filter(Boolean).join('\n'), tail(page.stderr));
  n.clean();
}
