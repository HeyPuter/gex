// A function too big for clang's memory stays out of its unit and runs on
// tier-1, with one build-log line. libbig.so's big() is about 3,000
// instructions of calls and branches, made here; flat() is as long but
// straight-line and trap-free, called by the trap-free flatcall(); small()
// is small. Three builds, each run against the host (glibc, host gcc):
//  - `--max-insns 1000`: big() and flat() are left out before clang runs;
//  - no limits: the unit takes them;
//  - `--clang-mem-mb M`: clang runs out of memory on their units, and the
//    retry leaves each unit's largest function out until the rest compiles.
//    M is found here, stepping up from a floor: the least limit clang
//    starts under that still has it run out on big()'s or flat()'s unit
//    (what it needs depends on the clang build). With no such limit the
//    case is left out, and said so.
// The unit holds small() every time.
//
// Not checked: that flat()'s trap-free caller writes everything back before
// calling the stub flat() becomes. A missed write-back shows only when the
// host reads the context during the call, and natively nothing does: flat()
// on tier-1 has no safepoint (no back edge) and no call that leaves.
import { execFileSync } from 'node:child_process';
import { rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { native, exec, tail } from '../lib/native.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], timeout: 1800 };

const BASE = 0x30000000;

/** big.c: big() of `n` statements, each a call, a compare and a branch over
 *  eight live locals (the shape of a big parser or table loader), flat(),
 *  flatcall() and small(). */
function bigC(n = 250) {
  let seed = 2;
  const rand = () => (seed = (seed * 1103515245 + 12345) % 2147483648) / 2147483648;
  const l = ['#include <string.h>',
    '__attribute__((noinline)) int get(void *p, int k) { return ((long *)p)[k & 7] + k; }',
    '__attribute__((noinline)) long put(void *p, int k, long v) { ((long *)p)[k & 7] ^= v; return v + k; }',
    'static const char *names[4] = {"k1", "k2", "k3", "k4"};',
    '__attribute__((noinline)) const char *name(void *p, int k) { return names[(((long *)p)[k & 7] + k) & 3]; }',
    'long small(long x) { return x * 3 + 1; }',
    // flat(): straight-line arithmetic, no call, no loop (trap-free), and
    // its trap-free caller flatcall(): under the limit flat() leaves, so
    // flatcall() must write back before calling it
    '__attribute__((noinline, visibility("hidden"))) long flat(long x, long y) {'];
  for (let k = 0; k < 400; k++) l.push(`  x ^= x << ${k % 29 + 1}; y += x >> ${k % 17 + 1}; x += y * ${k % 11 + 2};`);
  l.push('  return x ^ y;', '}', 'long flatcall(long x) { return flat(x, x + 7) + 1; }', 'long big(void *p, long *a) {',
    '  long x0 = a[0], x1 = a[1], x2 = a[2], x3 = a[3], x4 = a[4], x5 = a[5], x6 = a[6], x7 = a[7];');
  for (let k = 0; k < n; k++) {
    const v = `x${Math.floor(rand() * 8)}`, w = `x${Math.floor(rand() * 8)}`, r = rand();
    if (r < 0.4) l.push(`  if (strcmp(name(p, ${k}), "k${k % 4 + 1}") == 0) ${v} = put(p, ${k}, ${w} + ${k});`);
    else if (r < 0.7) l.push(`  if (get(p, ${k}) > ${Math.floor(rand() * 100)}) ${v} += ${w} * ${k % 13 + 1}; else ${w} ^= put(p, ${k}, ${v});`);
    else l.push(`  ${v} = ${w} + get(p, ${k}) * ${k % 7 + 1};`);
  }
  l.push('  return x0 + x1 + x2 + x3 + x4 + x5 + x6 + x7;', '}');
  return `${l.join('\n')}\n`;
}

export default async function (t) {
  const n = native(t);
  const d = n.dir('cgen-max-insns');
  writeFileSync(join(d, 'big.c'), bigC());
  await n.hostcc(['-O2', '-shared', '-fPIC', '-o', join(d, 'libbig.so'), join(d, 'big.c')]);
  await n.hostcc(['-O1', '-o', join(d, 'main'), n.src('cgen-max-insns/main.c'), `-L${d}`, '-lbig', "-Wl,-rpath,$ORIGIN"]);
  writeFileSync(join(d, 'pins.txt'), `0x${BASE.toString(16)} 0x10000 libbig.so\n`);
  const nm = execFileSync('nm', [join(d, 'libbig.so')], { encoding: 'utf8' });
  const at = (s) => `0x${(BASE + parseInt(nm.split('\n').find((l) => l.split(' ')[2] === s).split(' ')[0], 16)).toString(16)}`;
  const named = new RegExp(`(${at('big')}|${at('flat')}) \\(`);
  const host = (await exec([join(d, 'main')])).stdout;
  const image = (cache, max, mem) => n.gexc(['cgen-image', '--elf', join(d, 'libbig.so'), '--base', `0x${BASE.toString(16)}`, '--max-insns', String(max), '--clang-mem-mb', String(mem), '--jobs', '2'],
    { env: { GEX_LOG: '1', GEX_CGEN_CACHE: cache, GEX_CGEN_MAX_INSNS: String(max) }, timeout: 900000 });

  // The retry's limit: the first, stepping up, at which the build succeeds
  // and says clang ran out on big()'s or flat()'s unit. Built: clang
  // started; whether it also ran out decides (a larger limit would only run
  // out less).
  let mem = null;
  for (const m of [150, 200, 250, 300, 350, 400, 450, 500, 600, 700, 800]) {
    const cache = join(d, `probe-${m}`);
    const r = await image(cache, 0, m);
    rmSync(cache, { recursive: true, force: true });
    if (r.code !== 0) continue;
    const out = r.stdout + r.stderr;
    if (out.split('\n').some((l) => l.includes(`clang ran out of its ${m} MiB): `) && named.test(l))) mem = m;
    break;
  }
  const cases = [[1000, 8192, 'over 1000 instructions'], [0, 0, null]];
  if (mem) {
    t.say(`the retry case runs clang under ${mem} MiB`);
    cases.push([0, mem, `clang ran out of its ${mem} MiB`]);
  } else t.say("retry case left out: no clang memory limit both starts clang and makes it run out on big()'s or flat()'s unit");
  for (const [max, m, line] of cases) {
    const leg = `--max-insns ${max} --clang-mem-mb ${m}`;
    // the run looks for the unit under the key the same limit gives
    const cache = join(d, `cgen-${max}-${m}`);
    const b = await image(cache, max, m);
    if (!t.check(`${leg}: built`, b.code === 0, tail(b.stdout + b.stderr))) continue;
    const log = (b.stdout + b.stderr).split('\n');
    const said = line ? log.filter((l) => l.includes(`left to tier-1 (${line}): `) && named.test(l)).length : log.filter((l) => l.includes('left to tier-1')).length;
    t.check(`${leg}: ${line ? `big() or flat() left out (${line})` : 'nothing left out'}`, line ? said >= 1 : said === 0, log.filter((l) => l.includes('[cgen]')).slice(0, 4).join(' / '));
    const r = await n.gexc(['run', '--pin', join(d, 'pins.txt'), join(d, 'main')],
      { env: { GEX_LOG: '1', GEX_CGEN_CACHE: cache, GEX_ELF_UNITS: 'libbig.so', GEX_ELF_UNITS_BUILD: '0', GEX_CGEN_MAX_INSNS: String(max) }, timeout: 120000 });
    t.check(`${leg}: the unit placed`, /function tier for .*\/libbig\.so/.test(r.stderr), tail(r.stderr));
    t.eq(`${leg}: the run agrees with the host`, r.stdout, host, tail(r.stderr));
  }
  n.clean();
}
