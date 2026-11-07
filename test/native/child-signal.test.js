// A child process killed by a signal says so on gex's stderr, as a shell
// would: one no crash handler catches otherwise left only its absence. A
// crash (a signal that dumps core: an abort with no handler, a SIGSEGV) is
// said as one ("pid N (name) crashed: SIGSEGV ..."), a SIGKILL from another
// process as "pid N killed by signal 9". A pipeline's SIGPIPE and a session's
// SIGTERM say nothing, unless GEX_LOG_CHILD_SIGNALS=1 asks for every signal.
// Natively.
import { readFileSync } from 'node:fs';
import { native } from '../lib/native.mjs';

export const meta = { hosts: ['node'], timeout: 300 };

// Each child: its signal, and whether it is said by default (every one is
// with every signal asked for).
const cases = [[0, 13, false], [1, 6, true], [2, 15, false], [3, 11, true], [4, 9, true]];
const names = { 6: 'SIGABRT', 11: 'SIGSEGV' };

// Whether pid `pid`'s end by `signal` is said: a crash's line for a crash signal.
function saidOf(stderr, pid, signal) {
  if (names[signal]) return new RegExp(`\\[gex\\] pid ${pid}( \\([^)]*\\))? crashed: ${names[signal]}\\b`).test(stderr);
  return stderr.includes(`pid ${pid} killed by signal ${signal} `);
}

export default async function (t) {
  const n = native(t);
  const prog = n.g.cc({ name: 'cs', code: readFileSync(n.src('child-signal/cs.c'), 'utf8') }, { opt: '-O1' });
  for (const mode of ['default', 'all']) {
    const r = await n.gexc(['run', prog], { timeout: 120000, env: mode === 'all' ? { GEX_LOG_CHILD_SIGNALS: '1' } : {} });
    for (const [child, signal, byDefault] of cases) {
      const m = new RegExp(`^child ${child}: pid (\\d+) signal (\\d+)$`, 'm').exec(r.stdout);
      const said = !!m && saidOf(r.stderr, m[1], signal);
      const expect = mode === 'all' || byDefault;
      t.check(`${mode}: child ${child} got signal ${m?.[2] ?? '?'} (${signal}), said: ${said ? 'yes' : 'no'}`, m?.[2] === String(signal) && said === expect,
        `want said: ${expect ? 'yes' : 'no'}`);
    }
  }
}
