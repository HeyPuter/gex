// mkdir needs its parent, in both of the page runtime's filesystems (the
// single-Worker shim's own tmpfs, and the global kernel's through
// run-node.mjs): mkdir of a path whose parent is missing is ENOENT, under a
// file ENOTDIR. A `mkdir -p` (Qt's mkpath) tries the whole path first and
// makes the parents only on ENOENT; when that first mkdir succeeded, the
// parent was in no listing, `..` of the child did not resolve and realpath
// failed (KConfig's ~/.local/state, under a ~/.local never made).
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { guest, tail, guestLines, ROOT } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], timeout: 180 };

const WANT = 'mkdir /t/new/state with no /t/new: No such file or directory\nmkdir under a file: Not a directory\nok';

export default async function (t) {
  const g = guest(t);
  const shim = g.pub('gex_browser{infix}.wasm');
  g.pub('gex_kernel{infix}.plain.wasm');
  const prog = g.cc('mkdir-parent.c');
  const seed = g.seed({ shared: false });
  const f = join(t.workDir, 'guest', 'mkdir-parent.data');
  writeFileSync(f, 'data\n');

  // The single-Worker shim, alone, in this process.
  const gexMod = await import(join(ROOT, 'hosts/browser/js/gex.mjs'));
  if (g.arch === 'x86') {
    if (gexMod.setRealm) gexMod.setRealm({ arch: 'x86' });
    else globalThis.GEX_ARCH = 'x86';
  }
  let out = '';
  class Out extends gexMod.Env {
    write(fd, b) {
      if (fd !== 2) out += Buffer.from(b).toString();
    }
  }
  const gex = new gexMod.Gex(new Out());
  const entry = await gex.load(readFileSync(shim), readFileSync(seed), readFileSync(prog), { '/t/f': readFileSync(f) }, ['/mk'], { dirs: ['/t'] });
  await gex.run(entry);
  t.eq('the single-Worker shim', guestLines(out), WANT);

  // ProcessHost, with the global kernel's filesystem.
  const r = await g.run(seed, prog, [], { files: { '/t/f': f }, argv0: '/mk' });
  t.eq('ProcessHost', r.guest, WANT, tail(r.stderr, 3));
}
