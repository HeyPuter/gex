// A program whose heap grows past what its memory can hold: refused once
// (ENOMEM), it may ask for less and go on; one that asks again for as much
// is ended (SIGKILL) rather than left to retry for ever, its crash notice
// says why, and gex says the arena could not grow once, not once a try.
// fixtures/native/outgrow-heap/main.c, built static with the musl cross
// gcc, natively under this arch's gexc and on the page's shim.
import { native } from '../lib/native.mjs';

export const meta = { hosts: ['node'], timeout: 900 };

const lines = (out) => out.split('\n').filter((l) => l.startsWith('outgrow-heap ')).join('\n');

export default async function (t) {
  const n = native(t);
  const d = n.dir('outgrow-heap');
  const prog = `${d}/outgrow-heap`;
  await n.cc(['-O1', '-static', '-o', prog, n.src('outgrow-heap/main.c')]);
  const detail = (r) => `exit ${r.code}: ${(r.all ?? `${r.stdout}\n${r.stderr}`).split('\n').filter((l) => !/^\[host\]|^\[worker\] /.test(l)).slice(-6).join(' / ')}`;

  const spin = await n.gexc(['run', prog, 'spin']);
  t.eq('native: the second ask for as much ends it (SIGKILL)', [lines(spin.stdout), spin.code], ['outgrow-heap full (ENOMEM)', 137], detail(spin));
  t.check('native: its crash notice says gex ended it', /ended by gex: it asked again for 0x8000000 bytes/.test(spin.stderr), detail(spin));
  const fb = await n.gexc(['run', prog, 'fallback']);
  t.eq('native: one that asks for less goes on', [lines(fb.stdout), fb.code], ['outgrow-heap full (ENOMEM)\noutgrow-heap fallback ok', 0], detail(fb));

  const seed = n.seed();
  const run = (mode) => n.g.run(seed, prog, [mode], { files: { '/t/outgrow-heap': prog }, flags: ['--cwd', '/t'], argv0: './outgrow-heap', merge: true, env: { GEX_SHARED: '1' }, timeout: 300000 });
  const ps = await run('spin');
  t.eq('page: the second ask for as much ends it', [lines(ps.all), ps.code], ['outgrow-heap full (ENOMEM)', 137], detail(ps));
  const said = (ps.all.match(/the arena cannot grow/g) ?? []).length;
  t.check(`page: the arena's refusal said at most once (${said})`, said <= 1, detail(ps));
  const spent = /Spent: (\d+) MiB on guest mappings/.exec(ps.all);
  t.check(`page: what it spent on guest mappings fits its memory (${spent?.[1] ?? 'not said'} MiB)`, !spent || Number(spent[1]) <= 4096, detail(ps));
  const pf = await run('fallback');
  t.eq('page: one that asks for less goes on', [lines(pf.all), pf.code], ['outgrow-heap full (ENOMEM)\noutgrow-heap fallback ok', 0], detail(pf));
  n.clean();
}
