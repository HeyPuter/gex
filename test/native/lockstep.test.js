// The function tier's lockstep check: a guest run twice, on tier-1 and on the
// function tier (--cgen), each `gexc run --no-cache --no-jit` with
// GEX_SYSCALL_LOG and GEX_FAKE_CLOCK, and the logs compared thread by
// thread: every system call's number and arguments, the architectural
// registers at its entry, and its result; and the exit status and stdout.
// Tier-1 is the reference. Two comparisons for each program
// (fixtures/native/lockstep, static musl with zig: aarch64 on a64; x86-64
// and i386 on x86):
//   (a) the default build, the condition flags left out: both tiers leave
//       dead flags stale in the context, and differ in which stale values;
//       a live flag that is wrong shows up as a wrong branch, and so as a
//       divergence elsewhere;
//   (b) GEX_CGEN_LIVE=0, which writes every flag back everywhere, compared
//       exactly.
import { existsSync, readFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { exec, native, tail } from '../lib/native.mjs';
import { pool } from '../lib/gexcc.mjs';

export const meta = { hosts: ['node'], timeout: 1800 };

const PROGRAMS = ['ack', 'bits', 'branchy'];
const FIELDS = ['tid', 'pc', 'nr', 'a0', 'a1', 'a2', 'a3', 'a4', 'a5', '|', ...Array.from({ length: 31 }, (_, i) => `x${i}`), 'sp', 'flags', 'vhash'];

/** Per thread, its log lines' fields, in order (`flags: false` drops `f=` fields). */
function events(log, flags) {
  const per = new Map();
  for (const line of readFileSync(log, 'utf8').split('\n')) {
    let parts = line.split(/\s+/).filter(Boolean);
    if (!parts.length) continue;
    if (!flags) parts = parts.filter((x) => !x.startsWith('f='));
    if (!per.has(parts[1])) per.set(parts[1], []);
    per.get(parts[1]).push(parts);
  }
  return per;
}

function describe(a, b) {
  if (a[0] !== b[0]) return `line kinds differ: ${a[0]} vs ${b[0]}`;
  if (a[0] === 'R') return `result ${a[2]} vs ${b[2]}`;
  return a.slice(1).map((x, i) => (x !== b[i + 1] ? `${FIELDS[i] ?? `#${i}`}: ${x} vs ${b[i + 1]}` : null)).filter(Boolean).join(', ');
}

/** null when the two logs agree, else the first divergence. */
function compare(la, lb, flags) {
  const A = events(la, flags), B = events(lb, flags);
  for (const tid of [...new Set([...A.keys(), ...B.keys()])].sort((x, y) => Number(x) - Number(y))) {
    const xs = A.get(tid) ?? [], ys = B.get(tid) ?? [];
    const i = xs.findIndex((x, k) => k < ys.length && x.join(' ') !== ys[k].join(' '));
    if (i >= 0) return `thread ${tid}: diverges at event ${i} of ${xs.length}/${ys.length}: ${describe(xs[i], ys[i])}`;
    if (xs.length !== ys.length) return `thread ${tid}: A has ${xs.length} events, B ${ys.length}; the common prefix agrees`;
  }
  return null;
}

export default async function (t) {
  const n = native(t);
  const zig = n.g.tool('zig');
  const d = n.dir('lockstep');
  const targets = t.arch === 'x86' ? [['x86_64-linux-musl', []], ['x86-linux-musl', ['-Wl,--image-base=0x1000000']]] : [['aarch64-linux-musl', []]];
  const legs = [];
  for (const [target, more] of targets)
    for (const p of PROGRAMS) {
      const elf = join(d, `${p}-${target}`);
      const b = await exec([zig, 'cc', '-target', target, '-O2', '-static', '-ffp-contract=off', ...more, '-o', elf, n.src(`lockstep/${p}.c`), '-lm'], { env: n.env });
      if (!t.check(`${p} (${target}): builds`, b.code === 0, tail(b.stderr))) continue;
      for (const mode of ['a', 'b']) legs.push({ p, target, elf, mode });
    }
  await pool(legs, async ({ p, target, elf, mode }) => {
    const logs = ['', '--cgen'].map((f) => join(d, `${p}-${target}-${mode}${f}.log`));
    const env = mode === 'b' ? { GEX_CGEN_LIVE: '0' } : {};
    const runs = [];
    for (const [i, flags] of [[], ['--cgen']].entries()) {
      rmSync(logs[i], { force: true });
      runs.push(await n.gexc(['run', '--no-cache', '--no-jit', ...flags, elf], { env: { ...env, GEX_SYSCALL_LOG: logs[i], GEX_FAKE_CLOCK: '1' }, timeout: 600000 }));
    }
    const leg = `${p} (${target}, ${mode === 'a' ? 'flags left out' : 'GEX_CGEN_LIVE=0, exact'})`;
    if (!t.check(`${leg}: system call logs written`, logs.every((l) => existsSync(l)), `is this gexc built with the lockstep log? ${tail(runs[0].stderr)}`)) return;
    const why = compare(logs[0], logs[1], mode === 'b');
    t.check(`${leg}: the function tier agrees with tier-1`, why === null, why);
    t.check(`${leg}: exit and stdout agree`, runs[0].code === runs[1].code && runs[0].stdout === runs[1].stdout, `exit ${runs[0].code} vs ${runs[1].code}`);
    for (const l of logs) rmSync(l, { force: true });
  });
  n.clean();
}
