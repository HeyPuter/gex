// Wine's processes, started through the runtime API, number their blocks
// densely (hosts/browser/js/gex.mjs `#adoptExec`: an exec into a fresh Worker
// is dense unless its options say otherwise). The API never asks for dense
// blocks itself; with the classic dispatch tables instead, every Wine process
// held tables of millions of entries, which V8 keeps in the renderer's one
// trusted range, and the browser ran out of it. hello_args.exe under
// `log=tables`: the Workers' table lines must show `blocks` growing, and the
// classic dispatch tables past the first (the stubs' table) small.
import { bootWine } from '../lib/wine.mjs';

export const meta = { hosts: ['browser', 'node'], arches: ['x86'], images: ['wine64', 'win-progs'], timeout: 600 };

export default async function (t) {
  const w = await bootWine(t, { debug: { log: 'tables' } });
  await w.put('hello_args.exe');
  const r = await t.time('hello_args', () => w.run('hello_args.exe', ['a', 'b c']))
    .catch((e) => ({ code: -1, stdout: '', stderr: String(e.message) }));
  t.check('hello_args: exit 42', r.code === 42, `exit ${r.code}; stderr ${JSON.stringify(r.stderr.trim().split('\n').slice(-4).join(' / '))}`);
  await w.sys.shutdown();
  const lines = await t.logs(/\[tables\]/);
  const blocks = lines.filter((l) => /\bblocks=[1-9]/.test(l)).length;
  // The largest classic table any Worker held: dense, one Worker grows one to
  // about 9000 entries, none used; classic numbering held millions.
  const classic = Math.max(0, ...lines.flatMap((l) => [...l.matchAll(/\bdispatch[1-7]=(\d+)/g)].map((m) => Number(m[1]))));
  const at = lines.find((l) => l.includes(`=${classic}/`));
  t.check(`blocks dense in every process (${lines.length} table lines, ${blocks} with blocks, largest classic table ${classic})`,
    blocks > 0 && classic < 65536, at ?? (lines.slice(-2).join(' / ') || 'no [tables] line'));
}
