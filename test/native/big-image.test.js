// A program with more blocks than one wasm module may hold functions
// (wasmtime's million: gcc's cc1) runs, translated as it runs instead of
// ahead of time. GEX_IMAGE_MAX_FUNCS stands in for the limit, so a small
// program takes that path: it starts, forks, and its child execs it. And
// without it the same program is translated ahead of time as before.
// Natively.
import { mkdirSync } from 'node:fs';
import { native, count, trim, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 600 };

export default async function (t) {
  const n = native(t);
  const d = n.dir('big-image');
  mkdirSync(`${d}/root/proc`, { recursive: true });
  await n.cc(['-O1', '-static', '-o', `${d}/root/prog`, n.src('big-image/prog.c')]);
  for (const max of ['1', '']) {
    const leg = max ? 'past one module' : 'ahead of time';
    const r = await n.gexc(['run', '--no-cache', '--sysroot', `${d}/root`, `${d}/root/prog`], { env: { GEX_IMAGE_MAX_FUNCS: max } });
    t.eq(`${leg}: output`, trim(r.stdout), 'parent 116\nchild 116\nchild exit 0', tail(r.stderr));
    t.check(`${leg}: exits 0`, r.code === 0, `exit ${r.code}`);
    const said = count(r.stderr, /more than one module holds/);
    t.check(`${leg}: says it is past one module ${max ? 'at least once' : 'never'} (${said})`, max ? said >= 1 : said === 0, tail(r.stderr));
  }
  n.clean();
}
