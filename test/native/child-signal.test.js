// A child process killed by a crash signal says so on gex's stderr, as a
// shell would: one no crash handler catches (an abort with none installed, a
// SIGSEGV, a SIGKILL from another process) otherwise left only its absence.
// A pipeline's SIGPIPE and a session's SIGTERM say nothing, unless
// GEX_LOG_CHILD_SIGNALS=1 asks for every signal. Natively.
import { readFileSync } from 'node:fs';
import { native } from '../lib/native.mjs';

export const meta = { hosts: ['node'], timeout: 300 };

// Each child: its signal, whether it is said by default, and with every signal asked for.
const cases = [[0, 13, false], [1, 6, true], [2, 15, false], [3, 11, true], [4, 9, true]];

export default async function (t) {
  const n = native(t);
  const prog = n.g.cc({ name: 'cs', code: readFileSync(n.src('child-signal/cs.c'), 'utf8') }, { opt: '-O1' });
  for (const mode of ['default', 'all']) {
    const r = await n.gexc(['run', prog], { timeout: 120000, env: mode === 'all' ? { GEX_LOG_CHILD_SIGNALS: '1' } : {} });
    for (const [child, signal, byDefault] of cases) {
      const m = new RegExp(`^child ${child}: pid (\\d+) signal (\\d+)$`, 'm').exec(r.stdout);
      const said = !!m && r.stderr.includes(`pid ${m[1]} killed by signal ${signal} `);
      const expect = mode === 'all' || byDefault;
      t.check(`${mode}: child ${child} got signal ${m?.[2] ?? '?'} (${signal}), said: ${said ? 'yes' : 'no'}`, m?.[2] === String(signal) && said === expect,
        `want said: ${expect ? 'yes' : 'no'}`);
    }
  }
}
