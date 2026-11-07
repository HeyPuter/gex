// A native module built with an older gex_abi.h is refused at install, on
// the page's shim and natively (gexc run): its inline probe would read the
// probe cache in another layout and trap later at a wrong table index. The
// old header had no `gex.abi` custom section, so each case takes the
// install's module with that section renamed (same length, "gex.abX"): a
// module from before the marker. Cases: Wine's loader, then the i386 PE
// module (i386-windows/winepe.wasm), each in a hard-linked copy of the
// install, running syswow64's cmd.exe (so that the 32-bit PE module loads).
// Each must end with the refusal line, exit 127, and no trap; a refusal is
// gex declining to run the program, not its crash, so no crash is reported.
import { execFileSync } from 'node:child_process';
import { readFileSync, rmSync, writeFileSync, chmodSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { LOADER, wow64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1800 };

const PROG = ['C:\\windows\\syswow64\\cmd.exe', '/c', 'exit', '0'];

export default async function (t) {
  const w = wow64(t);
  // a copy of the install with `rel`, a module, from before the marker
  const old = (name, rel, drop = []) => {
    const d = join(w.work, `dist-${name}`);
    execFileSync('cp', ['-al', w.dist, d]);
    const b = readFileSync(join(w.dist, rel));
    const mark = Buffer.from('\x07gex.abi', 'latin1');
    if (b.indexOf(mark) < 0) throw new Error(`no gex.abi section in ${rel}: the install was built without the header marker`);
    for (const f of drop) rmSync(join(d, f), { force: true });
    rmSync(join(d, rel));
    let out = b, i;
    while ((i = out.indexOf(mark)) >= 0) out = Buffer.concat([out.subarray(0, i), Buffer.from('\x07gex.abX', 'latin1'), out.subarray(i + mark.length)]);
    writeFileSync(join(d, rel), out);
    chmodSync(join(d, rel), 0o755);
    return d;
  };
  const check = (leg, r, what) => t.check(`${leg}: refused, exit 127, no crash`,
    new RegExp(`${what} .*built for gex's native ABI 1, this gex's is`).test(r.out) && !/signature mismatch/i.test(r.out) && r.code === 127
      && !/\] pid \d+ .*crashed:/.test(r.out),
    w.detail(r));
  const pe = 'lib/wine/i386-windows/winepe.wasm';
  for (const [name, d, what, ld] of [
    ['loader', old('loader', LOADER), "Wine's loader", null],
    ['pe', old('pe', pe, readdirSync(join(w.dist, 'lib/wine/i386-windows')).filter((f) => /^winepe\..*\.cwasm$/.test(f)).map((f) => `lib/wine/i386-windows/${f}`)),
      "Wine's PE module", join(w.dist, LOADER)],
  ]) {
    check(`${name}, page`, await w.page(PROG, { dist: d, loader: ld ?? undefined, cwd: '/pfx/drive_c', name: `page-${name}` }), what);
    // natively Wine finds its modules beside the loader it runs
    check(`${name}, native`, await w.native(PROG, { loader: join(d, LOADER), name: `native-${name}` }), what);
    rmSync(d, { recursive: true, force: true });
  }
  w.clean();
}
