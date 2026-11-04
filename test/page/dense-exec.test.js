// A translated program that a native wasm program execs numbers its blocks
// densely, as it does started on its own (hosts/browser/js/gex.mjs `load`:
// the native program's own answer is not its children's run option). Wine's
// starter is a native wasm32 program that execs Wine's loader; when the
// starter's "no dense blocks" was handed down, the loader and every Wine
// process after it ran on classic dispatch tables, and a Wine boot stalled in
// wineboot. Three runs of an x86 program (GEX_LOG=tables): started directly,
// exec'd by a native program, and exec'd by one after `exec_fresh` (a new
// Worker and memory, as the starter does). Each must grow `blocks` and no
// classic dispatch table once the x86 program runs.
import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { guest, tail, ROOT } from '../lib/guest.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 420 };

const programs = new URL('../fixtures/programs/', import.meta.url);

export default async function (t) {
  const g = guest(t);
  const seed = g.pub('seed.x86.shared.wasm');
  if (!process.env.GEX_WASM_SYSROOT) return t.skip('GEX_WASM_SYSROOT (toolchain/sysroot/build32.sh)');
  const wcc = g.need(join(ROOT, 'wine/wcc'), 'wine/wcc');
  // The x86 program, against the host's static libc; the native starter, wcc's.
  const hi = g.cc({ name: 'dense-hi', code: readFileSync(new URL('dense-hi.c', programs), 'utf8') }, { glibc: true, flags: ['-static'] });
  const ds = join(t.workDir, 'guest', 'dense-spawn');
  const src = `${ds}.c`;
  writeFileSync(src, readFileSync(new URL('dense-spawn.c', programs), 'utf8'));
  if (!existsSync(ds)) execFileSync(wcc, ['-O2', '-o', ds, src], { stdio: ['ignore', 'ignore', 'pipe'] });
  for (const how of ['direct', 'plain', 'fresh']) {
    const [prog, args] = how === 'direct' ? [hi, [how]] : [ds, [how, '/t/hi', how]];
    const r = await g.run(seed, prog, args, {
      files: { '/t/ds': ds, '/t/hi': hi }, argv0: how === 'direct' ? '/t/hi' : '/t/ds', flags: ['--cwd', '/t'],
      env: { GEX_LOG: 'tables', GEX_SHARED: '1' }, merge: true,
    });
    const out = r.all;
    // The x86 program's lines: from its start or exec on.
    const from = out.search(/pid 1: (start|exec) \/t\/hi/);
    const x86 = from < 0 ? '' : out.slice(from);
    const blocks = (x86.match(/blocks grew/g) ?? []).length;
    const classic = (x86.match(/dispatch[1-7] grew/g) ?? []).length;
    const said = out.split('\n').filter((l) => l === `hi ${how}`).length;
    t.check(`${how}: the x86 program numbers its blocks densely (blocks grew ${blocks}, classic ${classic})`, said === 1 && blocks > 0 && classic === 0, tail(out, 8));
  }
}
