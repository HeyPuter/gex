// File times, in both of the page runtime's filesystems: the single-Worker
// shim's own tmpfs (`Gex` on its own, as an ejected project's run.mjs uses
// it), run with the syscall trace on and every block translated at run time
// -- the combination that re-entered the shim's kernel and panicked -- and
// the global kernel's, through run-node.mjs. A bundled file reports the
// host's mtime through both `stat` and `fstat` (gzip records it from
// `fstat`), a file the guest writes the time it was written, and
// `utimensat` sticks.
import { readFileSync, statSync, utimesSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { guest, tail, guestLines, ROOT } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], timeout: 180 };

export default async function (t) {
  const g = guest(t);
  const shim = g.pub('gex_browser{infix}.wasm');
  g.pub('gex_kernel{infix}.plain.wasm');
  const prog = g.cc('mtime.c');
  const seed = g.seed({ shared: false });
  const data = join(t.workDir, 'guest', 'mtime.data');
  writeFileSync(data, 'data\n');
  const when = Date.UTC(2021, 5, 15, 12) / 1000;
  utimesSync(data, when, when);
  const secs = Math.trunc(statSync(data).mtimeMs / 1000);
  const want = `bundled ${secs} ${secs}\nwritten now\nset 1234567890`;

  // The single-Worker shim, alone, in this process: tracing on.
  const gexMod = await import(join(ROOT, 'shims/browser/js/gex.mjs'));
  if (g.arch === 'x86') {
    if (gexMod.setRealm) gexMod.setRealm({ arch: 'x86' });
    else globalThis.GEX_ARCH = 'x86';
  }
  let out = '';
  let err = '';
  class Out extends gexMod.Env {
    write(fd, b) {
      if (fd === 2) err = (err + Buffer.from(b)).slice(-20000);
      else out += Buffer.from(b).toString();
    }
  }
  const gex = new gexMod.Gex(new Out());
  const entry = await gex.load(readFileSync(shim), readFileSync(seed), readFileSync(prog), { '/data': readFileSync(data) }, ['/times', '/data'],
    { dirs: ['/tmp'], mtimes: { '/data': secs }, strace: true });
  await gex.run(entry);
  t.eq('the single-Worker shim, traced', guestLines(out), want, err.split('\n').filter((l) => /panic|re-entered/i.test(l)).slice(0, 3).join(' / '));

  // ProcessHost, with the global kernel's filesystem.
  const r = await g.run(seed, prog, ['/data'], { files: { '/data': data }, argv0: '/times' });
  t.eq('ProcessHost', r.guest, want, tail(r.stderr, 3));
}
