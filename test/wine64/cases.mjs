// The case sets 64-bit Wine's tests share, natively and on the page
// (winerun.mjs's `nt`): each a function of a session `w` and `o`, the
// options every case gets (`page`, `env`), and a `pre` for its names.
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { Skip } from '../lib/t.js';
import { ROOT } from '../lib/winerun.mjs';
import { expect } from '../fixtures/wine/bigdll.mjs';

export const allOk = /(ok [^\n]*\n)+0 failures/;
export const hello = /hello from a Windows program with 3 args\narg 1: a\narg 2: b c/;
const F = join(ROOT, 'test/fixtures/wine64');

/** The core console programs (fixtures/wine/src). */
export const CONSOLE = [
  { name: 'hello_args', code: 42, want: hello, exe: 'hello_args.exe', args: ['a', 'b c'] },
  { name: 'sync', want: allOk, exe: 'sync.exe' },
  { name: 'crtmem', want: allOk, exe: 'crtmem.exe' },
  { name: 'apc', want: allOk, exe: 'apc.exe' },
  { name: 'tls', want: allOk, exe: 'tls.exe' },
  { name: 'cxx', want: allOk, exe: 'cxx.exe' },
  { name: 'exitproc', code: 33, want: /worker exiting\natexit ran/, exe: 'exitproc.exe' },
  { name: 'seh', code: 77, want: /(ok [^\n]*\n)+top-level filter saw e00000ff\n0 failures/, exe: 'seh.exe' },
  { name: 'seh-traps', want: allOk, exe: 'seh.exe', args: ['traps'] },
];

/** A locked write through a null pointer's field: an access violation for
 *  the program's handler, a write at that address. */
export const LOWNULL = ['xadd', 'aligned', 'cmpxchg16b'].map((k) => ({ name: `lownull-${k}`, want: allOk, exe: 'lownull.exe', args: [k] }));

/** KUSER_SHARED_DATA read by constant and through a computed pointer.
 *  `compact`: a 64-bit Wine process in a 32-bit memory's compact space
 *  (the page with the native PE core), where Windows' page is folded and
 *  the computed pointer is an access violation at its address; elsewhere
 *  the page is there. */
export const kuser = (compact) => [
  { name: 'kuser', want: allOk, exe: 'kuser.exe', args: ['const'] },
  compact
    ? { name: 'kuser-pointer', want: /ok pointer read at 0x7ffe026c: exception 0xc0000005, read 0, address 0x7ffe026c\n0 failures/, exe: 'kuser.exe', args: ['pointer'] }
    : { name: 'kuser-pointer', code: 3, want: /read [0-9]+ at 0x7ffe026c/, exe: 'kuser.exe', args: ['pointer'] },
];

/** USER on the null driver: window procs entered from NtUser* calls,
 *  messages between threads (user_win), a window proc's exceptions
 *  (wndproc_seh, wndproc_uncaught), two levels of calls into Wine with
 *  traps out of the inner one (nestsp). */
export const NULLDRV = ['user_win', 'wndproc_seh', 'nestsp', 'wndproc_uncaught'].map((n) => ({ name: n, want: allOk, exe: `${n}.exe`, nullDriver: true }));

/** Run `cases` with `o` on top, each named `pre` + its name. */
export async function each(w, cases, o = {}, pre = '') {
  for (const c of cases) await w.nt({ ...c, ...o, env: { ...o.env, ...c.env }, name: `${pre}${c.name}` });
}

/**
 * The page's first Wine process starts the server and has no door: it runs
 * its program again in its place (a new Worker and memory, the same pid),
 * so the status is that process's own, never one a waiting child's exit
 * passes on (`[gex] process N exited 42`, N not 1, GEX_DEBUG_EXIT), which
 * would hold its own memory for the whole session. Run with the door on.
 */
export async function starter(t, w, o, pre = '') {
  const r = await w.nt({ ...CONSOLE[0], ...o, name: `${pre}starter`, wineEnv: { WINEGEX_DOOR: '1' }, env: { ...o.env, GEX_DEBUG_EXIT: '1' } });
  if (r) t.check(`${pre}starter-exec: the program ran in the first process, not a child of it`,
    !/^\[gex\] process ([02-9]|[1-9][0-9]+)(:[0-9]+)? exited 42/m.test(r.err), w.detail(r, 4));
}

/**
 * 24 DLLs of a thousand functions each loaded, run and unloaded one after
 * another, as a plugin scan does (bigdll.c's plugNN). `rss` (the page): run
 * once and `rounds` times; the peak RSS must grow by under `mb` MiB per
 * further unloaded image, and an image loaded again at its base is
 * numbered as before (under a page of dispatch slots per further load).
 */
export async function dllcycle(t, w, o, pre = '', { rss = false, rounds = 4, mb = 18 } = {}) {
  const extra = Array.from({ length: 24 }, (_, i) => w.prog(`plug${String(i).padStart(2, '0')}.dll`));
  const one = (name, n, more = {}) => w.nt({ ...o, name: `${pre}${name}`, want: new RegExp(`dllcycle ${24 * n} x ${expect(1000, 24 * n)}`),
    exe: 'dllcycle.exe', args: ['24', String(n)], extra, ...more });
  if (!rss) return one('dllcycle', 1);
  const env = { ...o.env, GEX_TIME_JIT: '1' };
  const page = { ...o.page, rss: true };
  const r1 = await one('dllcycle', 1, { env, page });
  const rR = await one('dllcycleR', rounds, { env, page, timeout: 300000 * rounds });
  if (!r1?.rss || !rR?.rss || r1.code !== 0 || rR.code !== 0) return;
  const loads = 24 * (rounds - 1);
  const per = (rR.rss - r1.rss) / loads / 1024;
  t.check(`${pre}dllcycle-rss: ${per.toFixed(1)} MiB per further unloaded image (rss ${r1.rss} -> ${rR.rss} KiB, 24 -> ${24 * rounds} loads), want under ${mb}`, per < mb);
  const left = (r) => Math.min(...[...r.err.matchAll(/slots left ([0-9]+)k/g)].map((m) => Number(m[1])));
  const l1 = left(r1), l2 = left(rR);
  if (t.check(`${pre}dllcycle-slots: slot counts in both runs`, Number.isFinite(l1) && Number.isFinite(l2))) {
    const each = Math.floor(((l1 - l2) * 1000) / loads);
    t.check(`${pre}dllcycle-slots: ${each} dispatch slots per further load (${l1}k -> ${l2}k left), want under 4096`, each < 4096);
  }
}

/**
 * The ASIO host (asiohost.c) on gex audio: Wine's dlls/gexasio, registered,
 * created by its class id and run for 10 s of bufferSwitch, the switch
 * count within 10% of the rate's. On the page the device has capture:
 * with run-node's player and its loopback (`o.env` GEX_AUDIO_PLAYER=1
 * GEX_AUDIO_LOOP=1) the inputs bring an impulse back, its round trip timed.
 */
export async function asio(t, w, o, pre = '') {
  const dll = join(w.dist, 'lib/wine/x86_64-windows/gexasio.dll');
  if (!w.loaderHas('gexasio.so')) return t.say(`${pre}asio: no gexasio in this install (Wine's dlls/gexasio)`);
  const rt = o.page ? '\nasio rt frames [0-9 ]+\\([0-9]+ of [0-9]+\\) ms [0-9.]+' : '';
  // (gexasio.dll beside the program: the prefix predates it, and Wine loads
  // a builtin only through its file in the prefix, as wineboot installs it)
  await w.nt({ ...o, name: `${pre}asio`, exe: 'asiohost.exe', extra: [dll],
    want: new RegExp(`asio driver gex audio\nasio rate [0-9]+ buffer 512 \\(min 64 max 2048\\) inputs [02] outputs 2 latency in [0-9]+ out [0-9]+\nasio switches [0-9]+ want [0-9]+ \\(first after [0-9]+ ms\\), position ok, time info steady${rt}`) });
}

/**
 * The translation cache across loads (the page only) for code Wine reads
 * into memory: a DLL whose sections are not page-aligned in its file
 * (MinGW's 512-byte alignment) is read, relocated and protected, and named
 * by its bytes at that address. A first run saves the cache, a second
 * restores it and lifts under 5% of the first's blocks in the DLL, and a
 * run that loads it at another base gets none of the first base's
 * translations (at least half the blocks again). A "[gex] xcache ...
 * saved" line may come between any two of the program's lines.
 */
export async function xcache(t, w, o, pre = '') {
  const dll = w.prog('xcbig.dll');
  const dir = join(w.work, 'xcdir');
  const env = { ...o.env, GEX_XCACHE: 'wine64', GEX_XCACHE_DIR: dir, GEX_XCACHE_MIN: '0', GEX_TIME_JIT: '1' };
  const got = `x=${expect(1000, 1)} base=[0-9A-Fa-f]+ size=[0-9a-f]+`;
  const any = '([^\\n]*\\n)*', tail = '(\\n[^\\n]*)*';
  const run = (name, want, args = []) => w.nt({ ...o, env, name: `${pre}${name}`, exe: 'xcload.exe', args: ['xcbig.dll', ...args], extra: [dll], want: new RegExp(want) });
  const runs = [await run('xcache-save', `${any}${got}${tail}`), await run('xcache-restore', `${any}${got}${tail}`),
    await run('xcache-moved', `${any}first base=[0-9A-Fa-f]+\n${any}${got}${tail}`, ['move'])];
  if (runs.some((r) => !r || r.code !== 0)) return;
  // the blocks a run lifted in the DLL: its process's objects at the address
  // it printed (each object as name=blocks@first..last)
  const blocks = (r) => {
    const m = [...r.stdout.matchAll(/x=\d+ base=([0-9A-Fa-f]+) size=([0-9a-f]+)/g)].pop();
    if (!m) return [0, null];
    const lo = parseInt(m[1], 16), hi = lo + parseInt(m[2], 16);
    let n = 0;
    for (const l of r.err.split('\n')) {
      if (!l.includes(' pid ') || !l.includes('translating')) continue;
      for (const [, b, a] of l.matchAll(/=(\d+)@0x([0-9a-f]+)\.\./g)) if (lo <= parseInt(a, 16) && parseInt(a, 16) < hi) n = Math.max(n, Number(b));
    }
    return [n, lo];
  };
  const [[b1, at1], [b2, at2], [b3, at3]] = runs.map(blocks);
  const restored = /restored [1-9][0-9]* modules/.exec(`${runs[1].out}\n${runs[1].err}`)?.[0];
  t.check(`${pre}xcache: the DLL's blocks lifted: ${b1} at ${at1?.toString(16)}, ${b2} at ${at2?.toString(16)} after the cache (${restored}), ${b3} at ${at3?.toString(16)}; want under 5% of the first, then at least half at another base`,
    b1 > 1000 && at2 === at1 && !!restored && b2 < b1 / 20 && at3 !== null && at3 !== at1 && b3 >= b1 / 2);
}

/**
 * gexcc-win programs (native wasm modules beside their .exe), built with
 * GEX_TEST_GEXCC_WIN (a skip without it): `only` the names to run (all by
 * default). gexccwin.test.js says what each checks.
 */
export async function gexccWin(t, w, o = {}, pre = '', only = null) {
  const W = join(ROOT, 'test/fixtures/wine/src');
  const want = (n) => !only || only.includes(n);
  const one = async (name, src, args, c = {}) => {
    if (!want(name)) return;
    let exe;
    try { exe = await w.gexccWin(src, `${name}.exe`, args); } catch (e) {
      if (e instanceof Skip) throw e;
      return t.check(`${pre}${name}: build`, false, String(e.message));
    }
    const { extra = [], ...more } = c;
    await w.nt({ want: allOk, ...o, name: `${pre}${name}`, exe, extra: [`${exe}.wasm`, ...extra], ...more });
  };
  await one('cxxcatch', join(F, 'cxxcatch.cpp'), ['-O2']);
  await one('dllimps', join(F, 'dllimps.c'), ['-O2']);
  await one('cxxthr', join(F, 'cxxthr.cpp'), ['-O2']);
  await one('timerproc', join(W, 'timerproc.c'), ['-O1', '-municode', '-lcomctl32'], { nullDriver: true });
  await one('lresult', join(F, 'lresult.c'), ['-O1', '-mwindows'], { nullDriver: true });
  await one('cbfault', join(F, 'cbfault.c'), ['-O1'], { nullDriver: true,
    want: /cbfault: sending\ncbfault: veh c0000005\n([^\n]*\n)*cbfault: returned 0, veh [0-9]+/ });
  await one('ptval', join(F, 'ptval.c'), ['-O1', '-mwindows'], { nullDriver: true });
  await one('hostdatacc', join(W, 'hostdata.c'), ['-O2']);
  if (want('wmain0')) {
    // wmain(void) compiled and linked in one step
    const w0 = join(w.work, 'wmain0.c');
    writeFileSync(w0, '#include <stdio.h>\nint wmain(void) { printf("wmain0 ok\\n"); return 40; }\n');
    await one('wmain0', w0, ['-O1', '-municode'], { code: 40, want: /wmain0 ok/ });
  }
  if (want('wmain3')) {
    // wmain(argc, argv, envp) compiled first (-c), then linked
    const w3 = join(w.work, 'wmain3.c');
    writeFileSync(w3, '#include <stdio.h>\nint wmain(int c, unsigned short **v, unsigned short **e) { printf("wmain3 ok %d %s\\n", c, e && e[0] ? "env" : "noenv"); return 43; }\n');
    const o3 = await w.gexccWin(w3, 'wmain3.o', ['-O1', '-c']);
    await one('wmain3', o3, ['-municode'], { code: 43, want: /wmain3 ok 1 env/ });
  }
  if (!want('xthr') && !want('xhoist')) return;
  // xthr_dll.dll beside both
  let dll;
  try { dll = await w.gexccWin(join(F, 'xthr_dll.c'), 'xthr_dll.dll', ['-O2', '-shared', join(F, 'xthr_dll.def')]); } catch (e) {
    if (e instanceof Skip) throw e;
    return t.check(`${pre}xthr_dll: build`, false, String(e.message));
  }
  for (const n of ['xthr', 'xhoist']) await one(n, join(F, `${n}.c`), ['-O2'], { extra: [dll, `${dll}.wasm`] });
}
