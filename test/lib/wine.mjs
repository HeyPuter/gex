// 64-bit Wine composed from the runtime's classes, as a page does it (images
// `wine64` and `win-progs`): its install at /wine64, the page prefix at /pfx,
// the test programs at /progs/{x64,x86}. Then `run(exe, args)` copies a test
// program into C:\ and runs it with Wine: `{ code, stdout, stderr }`, the
// output's line ends Unix ones.
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';

export const WINE = '/wine64/lib/wine/x86_64-unix/wine';

/** `env`: more of Wine's environment; `parts(sys, fs)`: the page's other
 *  classes, made before boot; `system`: more of System's options (`units`,
 *  `xcache`) and `debug` its switches; `wow64`: 32-bit programs through WoW64 (the
 *  install's i386-windows); `nullDriver`: USER with no display (Wine's null
 *  graphics driver; WoW64's default). */
export async function bootWine(t, { env = {}, parts, wow64 = false, nullDriver = wow64, system = {}, debug = {} } = {}) {
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug(debug), ...system });
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('wine64.tar'), { lazy: t.work('wine64-lazy.json') });
  fs.addTar(t.work('win-progs.tar'), { at: '/progs' });
  const more = parts?.(sys, fs);
  await t.timeout(sys.boot(), 120000, 'boot');
  const failed = JSON.parse(await (await fetchWork(t, 'win-progs.json')).text()).failed;
  const wineEnv = { WINEPREFIX: '/pfx', WINEDEBUG: 'err+all,fixme-all', WINEGEX_DOOR: '1', ...(wow64 && { WINEGEX_WOW64: '1' }), ...env };
  if (wow64) {
    try { await fs.stat('/wine64/lib/wine/i386-windows'); } catch { t.skip('the Wine install has no lib/wine/i386-windows: not a WoW64 build'); }
  }
  if (nullDriver) await fs.writeFile('/pfx/user.reg', '\n[Software\\\\Wine\\\\Drivers]\n"Graphics"="null"\n', { append: true });
  /** Copy test program or DLL `exe` (in /progs/ARCH) into C:\. */
  async function put(exe, arch = 'x64') {
    const why = failed[`${arch}/${exe.replace(/\.(exe|dll)$/, '.c')}`] ?? failed[`${arch}/${exe.replace(/\.(exe|dll)$/, '.cpp')}`];
    if (why) throw new Error(`${arch}/${exe} did not build: ${why}`);
    await fs.writeFile(`/pfx/drive_c/${exe}`, await fs.readFile(`/progs/${arch}/${exe}`));
  }
  /** Run test program `exe` (in /progs/ARCH) from C:\. */
  async function run(exe, args = [], { arch = 'x64', timeout = 300000, env: e = {} } = {}) {
    await put(exe, arch);
    const r = await t.timeout(sys.run([WINE, `C:\\${exe}`, ...args], { env: { ...wineEnv, ...e }, cwd: '/pfx/drive_c' }), timeout, exe);
    return { ...r, stdout: r.stdout.replace(/\r/g, '') };
  }
  /** Start Wine's server (`wineserver -f -p`) from the page, before any
   *  program: resolves with its Process once it is up, as Wine's loader
   *  waits for it (ntdll start_server): a byte on the descriptor
   *  WINEGEX_SERVER_READY names, here its stdout. Its socket alone is not
   *  enough: a client that comes before the server's door is up cannot
   *  publish the shared user data, and wineboot copies an empty one. */
  async function startServer({ timeout = 120000 } = {}) {
    const p = sys.spawn([WINE.replace(/wine$/, 'wineserver'), '-f', '-p'], { env: { ...wineEnv, WINEGEX_SERVER_READY: '1' } });
    // Read, not cancelled: a write to a closed pipe would end it (SIGPIPE).
    p.stderr.pipeTo(new WritableStream()).catch(() => {});
    const rd = p.stdout.getReader();
    const ready = rd.read().then(({ value, done }) => {
      if (done || !value?.length) throw new Error('wineserver ended before it was up');
      rd.releaseLock();
      p.stdout.pipeTo(new WritableStream()).catch(() => {});
    });
    await t.timeout(Promise.race([ready, p.exited.then((st) => { throw new Error(`wineserver exited (${JSON.stringify(st)}) before it was up`); })]), timeout, 'wineserver up');
    return p;
  }
  return { sys, fs, run, put, startServer, env: wineEnv, more };
}

function fetchWork(t, name) {
  const u = t.work(name);
  if (u.startsWith('file:')) return import('node:fs/promises').then((f) => f.readFile(new URL(u))).then((b) => new Response(b));
  return fetch(u);
}

/** What the runtime said of each process as it exited, under the debug
 *  switch `log: 'debug'` (host.mjs, "[gex] pid N: lived ..."): its dispatch
 *  slots left (the page's budget, in thousands), memory and the blocks it
 *  translated by object. A process may be said twice (its last thread). */
export async function exitStats(t) {
  return (await t.logs(/\[gex\] pid [0-9:]+: lived /)).map((l) => ({
    // `P`, or `P:T` for a thread T's line (no trailing colon).
    pid: /pid (\d+(?::\d+)*): lived/.exec(l)[1],
    cmd: /\{([^}]*)\}/.exec(l)?.[1] ?? '',
    slotsLeftK: Number(/(\d+)k dispatch slots left/.exec(l)?.[1] ?? NaN),
    memoryMib: Number(/memory (\d+) MiB/.exec(l)?.[1] ?? NaN),
    kernelMib: Number(/kernel (\d+) MiB/.exec(l)?.[1] ?? NaN),
    tables: /tables blocks (\d+) ftab (\d+)/.exec(l)?.slice(1).map(Number) ?? null,
    // name=blocks@first..last
    objects: [...l.matchAll(/(\S*)=(\d+)@0x([0-9a-f]+)\.\.0x([0-9a-f]+)/g)].map((m) => ({ name: m[1], blocks: Number(m[2]), first: parseInt(m[3], 16) })),
  }));
}

/** Wait (up to `ms`) for the runtime's exit line of the process whose
 *  command line matches `cmd`: the process's own (`pid P`), which comes
 *  after its threads' (`pid P:T`). A program's exit reaches the page (its `run` ends)
 *  before its Worker has saved the translation cache and sent the numbers
 *  its exit line says; a shutdown before then ends the Worker without that
 *  line. True once it is there. */
export async function exitLineOf(t, cmd, ms = 60000) {
  for (const t1 = Date.now() + ms; Date.now() < t1; await new Promise((r) => setTimeout(r, 200))) {
    if ((await exitStats(t)).some((s) => !s.pid.includes(':') && cmd.test(s.cmd))) return true;
  }
  return false;
}

/** The blocks translated in the objects that start in any of `ranges`
 *  (`[lo, hi)` pairs): each object once, at the most any line says. */
export function blocksIn(stats, ranges) {
  const most = new Map();
  for (const s of stats) {
    for (const o of s.objects) {
      if (!ranges.some(([lo, hi]) => o.first >= lo && o.first < hi)) continue;
      const k = `${o.name}@${o.first}`;
      most.set(k, Math.max(most.get(k) ?? 0, o.blocks));
    }
  }
  return [...most.values()].reduce((a, b) => a + b, 0);
}

/** The ranges xcload.exe and xcload2.exe print ("base=B size=S"). */
export function printedRanges(stdout) {
  return [...stdout.matchAll(/base=([0-9A-Fa-f]+) size=([0-9a-f]+)/g)].map((m) => [parseInt(m[1], 16), parseInt(m[1], 16) + parseInt(m[2], 16)]);
}
