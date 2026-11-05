// Faults nested in their own dispatch without end (fault-nest.c) on the
// page (run-node.mjs), on the function JIT (whose faults are delivered in
// place, under the frames that faulted) and on tier-1: the process must end
// as Windows ends one whose stack is spent -- Wine's "stack overflow" or
// gex's (exit 253, when the host's own stack has no room for another
// level) -- never hang, run below its stack, or crash the runtime. Each
// with a Worker stack as small as a browser's (GEX_WORKER_STACK_MB=1), and
// one too small for a second level in place (0.4: each from the run loop).
// The callback case nests a window procedure's call-out in every level.
// With GEX_TEST_WINE32_DIST and _PREFIX (32-bit Wine and a prefix of its
// own) the plain case on 32-bit Wine too.
import { graphics, wine64 } from '../lib/winerun.mjs';

export const meta = { hosts: ['node'], arches: ['x86'], images: ['win-progs'], timeout: 5400 };

export default async function (t) {
  const w = wine64(t, { door: 0 });
  const env = process.env;
  const w32 = env.GEX_TEST_WINE32_DIST && env.GEX_TEST_WINE32_PREFIX ? { dist: env.GEX_TEST_WINE32_DIST, prefix: env.GEX_TEST_WINE32_PREFIX } : null;
  // With the native PE side each level costs the host's stack (ntdll's
  // dispatcher calls the handler out through a call-out whose native frames
  // stay under it), and the process ends after fewer levels: in the small
  // stack about 58, the plain case's 1 MiB about 455, the callback case's
  // about 182. The floors, about 80% of those, leave room for a loaded host.
  const floor = (name) => !w.nativeCore ? 100 : name.endsWith('-small') ? 46 : /^plain-.*-1m$/.test(name) ? 360 : /^callback-.*-1m$/.test(name) ? 145 : 100;
  const one = async (name, mode, more = {}) => {
    const on32 = name.endsWith('-w32');
    const exe = w.prog('fault-nest.exe', on32 ? 'x86' : 'x64');
    const r = await w.page(['C:\\fault-nest.exe', mode], { pfxFiles: { 'fault-nest.exe': exe }, userReg: graphics('null'), cwd: '/pfx/drive_c',
      wineEnv: { WINEGEX_NO_WINEBOOT: '1' }, env: more, timeout: 600000, name,
      ...(on32 && { dist: w32.dist, prefix: w32.prefix, rel: 'lib/wine/i386-unix/wine' }) });
    const text = r.out.replace(/\r/g, '');
    const levels = Number([...text.matchAll(/^nest ([0-9]+)/gm)].pop()?.[1] ?? 0);
    let why = '';
    if (!text.includes(`fault-nest ${mode} start`)) why = 'no start line';
    if (r.timedOut) why = 'hung';
    if (/trapped inside the shim|Maximum call stack size/.test(text)) why = 'the runtime crashed';
    if (text.includes('LIVELOCK')) why = 'ran below its stack (a livelock)';
    if (/^fault-nest survived/m.test(text)) why = 'survived';
    if (!why && !text.includes('stack overflow')) why = `no stack overflow said (exit ${r.code})`;
    if (!why && levels < floor(name)) why = `only ${levels} levels (want ${floor(name)})`;
    if (!why && name.endsWith('-small') && !text.includes("the host's stack short")) why = 'no level delivered from the run loop for want of stack';
    t.check(`${name}: ${levels} levels, exit ${r.code}, a stack overflow`, !why, `${why}; ${w.detail(r, 4)}`);
  };
  await one('plain-fjit', 'plain', { GEX_FJIT: '1' });
  await one('plain-fjit-1m', 'plain', { GEX_FJIT: '1', GEX_WORKER_STACK_MB: '1' });
  await one('plain-fjit-small', 'plain', { GEX_FJIT: '1', GEX_WORKER_STACK_MB: '0.4' });
  await one('callback-fjit', 'callback', { GEX_FJIT: '1' });
  await one('callback-fjit-1m', 'callback', { GEX_FJIT: '1', GEX_WORKER_STACK_MB: '1' });
  await one('plain-tier1', 'plain');
  if (!w32) t.say('32-bit Wine: GEX_TEST_WINE32_DIST and _PREFIX not given, its cases not run');
  else {
    await one('plain-fjit-w32', 'plain', { GEX_FJIT: '1' });
    await one('plain-tier1-w32', 'plain');
  }
  w.clean();
}
