// The runtime's switches, each in one row: the name a Worker's start message
// carries it under, the global the runtime reads it from, the page's URL key
// and the environment variable run-node.mjs takes, and what kind of value it
// is. Every page parses its URL with `pageOptions`, runtime.js puts what was
// set into the start message (`startOptions`), worker.mjs sets the globals
// again in each Worker (`workerOptions`), and run-node.mjs reads the
// environment (`envOptions`).
//
// Kinds:
//   flag    on when given at all (`?x=1`, `?x=yes`), off when not
//   one     on only as `1`
//   num     a number, left out when 0
//   num0    a number, 0 kept
//   numor1  a number, 1 for a value that is not one (`?doorstats=on`)
//   str     a string

/** `to`: where the start goes, `extra` (every Worker of every process) or
 *  `opts` (pid 1's, which its children inherit). `worker`: worker.mjs sets
 *  the global from it, for code that reads the global in a Worker. */
export const OPTIONS = [
  // name            global                  url              env                      kind      to       worker
  // The engine's stack for a trap, which names the shim function the fault
  // was in.
  ['trapStack',      'GEX_TRAP_STACK',       'trapstack',     'GEX_TRAP_STACK',        'flag',   'extra', true],
  // Wineserver door call counts every 10 s (a number past 1: the line's
  // period in ms).
  ['doorStats',      'GEX_DOOR_STATS',       'doorstats',     'GEX_DOOR_STATS',        'numor1', 'extra', true],
  // The function probe cache's misses, every 10 s (cgen.rs `fcache_stat`).
  ['fcacheStats',    'GEX_FCACHE_STATS',     'fcachestats',   'GEX_FCACHE_STATS',      'flag',   'extra', false],
  // Label process and thread Worker exits, without the debug option's
  // mirroring of every guest write.
  ['exitTrace',      'GEX_EXIT_TRACE',       'exittrace',     null,                    'flag',   'extra', false],
  ['calloutTrace',   'GEX_CALLOUT_TRACE',    'callouttrace',  'GEX_CALLOUT_TRACE',     'flag',   'extra', true],
  // A return to native Wine's pad past open call-outs stays a breakpoint
  // (cgen.rs STRAY_TRAP).
  ['strayTrap',      'GEX_STRAY_TRAP',       'straytrap',     'GEX_STRAY_TRAP',        'flag',   'extra', true],
  // Wine processes take the 2 GiB split (the shim's heap above 2 GiB), not
  // the compact space: for a game that needs all of Windows' 2 GiB.
  ['wine32Split',    'GEX_WINE32_SPLIT',     'winesplit',     'GEX_WINE32_SPLIT',      'flag',   'extra', true],
  // Guest accesses below 64 KiB fault, as a null pointer's do natively
  // (gex_abi::null_guard). A debugging opt-in: off, tier 1 and the units are
  // as without it; on, 2-8% slower in tier 1.
  ['nullGuard',      'GEX_NULL_GUARD',       'nullguard',     'GEX_NULL_GUARD',        'one',    'extra', true],
  // 64-bit Wine: a system call or WoW64 thunk that goes through a bad
  // pointer ends with STATUS_ACCESS_VIOLATION, as on Windows, not the
  // process (gex.native_try). A debugging opt-in: off, Wine's system calls
  // are as without it.
  ['nativeTry',      'GEX_NATIVE_TRY',       'nativetry',     'GEX_NATIVE_TRY',        'one',    'extra', true],
  // SSE honours MXCSR's rounding control in every tier (gex_ir::mxcsr_exact),
  // for a program that needs it. Off, SSE rounds to nearest, and tier 1 and
  // the units are as without it.
  ['mxcsr',          'GEX_MXCSR',            'mxcsr',         'GEX_MXCSR',             'one',    'extra', true],
  // Bytes of address space above the placement window for the project's
  // libraries, each at its build's base (gex_abi::set_pin_region).
  ['pinRegion',      'GEX_PIN_REGION',       'pinregion',     'GEX_PIN_REGION',        'num',    'extra', true],
  // A crash report names the SEH chain and a longjmp's jmp_buf.
  ['sehReport',      'GEX_SEH_REPORT',       'sehreport',     'GEX_SEH_REPORT',        'one',    'extra', true],
  // The processors every process is told it has (`gex_kernel::cpus`), in
  // place of navigator.hardwareConcurrency: for a page that wants fewer.
  ['cpus',           'GEX_CPUS',             'cpus',          'GEX_CPUS',              'num',    'extra', false],
  // Benchmarks only: the monotonic clocks N times real speed, the wall clock
  // starting at Unix time S.
  ['timeScale',      'GEX_TIME_SCALE',       'timescale',     null,                    'num',    'extra', true],
  ['fixedTime',      'GEX_FIXED_TIME',       'fixedtime',     null,                    'num',    'extra', true],
  // A forked child does not copy any inherited region larger than N MiB,
  // leaving those pages zero: a destructive diagnostic (`adoptProcess`).
  ['forkSkip',       'GEX_FORK_SKIP_MIB',    'forkskip',      null,                    'num0',   'extra', true],
  // How many traps a forked child takes before it stops waiting to be an
  // execve and takes its parent's modules in bulk.
  ['adoptAfter',     'GEX_ADOPT_AFTER',      'adopt',         'GEX_ADOPT_AFTER',       'num0',   'extra', true],
  ['adoptTrace',     'GEX_ADOPT_TRACE',      'adopttrace',    'GEX_ADOPT_TRACE',       'flag',   'extra', true],
  ['attachTrace',    'GEX_ATTACH_TRACE',     'attachtrace',   'GEX_ATTACH_TRACE',      'flag',   'extra', true],
  ['denseDebug',     'GEX_DENSE_DEBUG',      'densedebug',    'GEX_DENSE_DEBUG',       'flag',   'extra', true],
  ['traceMisses',    'GEX_TRACE_MISSES',     'tracemisses',   'GEX_TRACE_MISSES',      'flag',   'extra', false],
  // Blocks one dispatch miss may lift (default 2048); dispatch inline in
  // every block; a translator progress line every N translations; tier-1's
  // tee folding (gex-emit `set_t1_fold`).
  ['liftMax',        'GEX_LIFT_MAX',         'liftmax',       'GEX_LIFT_MAX',          'num',    'extra', false],
  ['inlineDispatch', 'GEX_INLINE_DISPATCH',  'inlinedispatch', 'GEX_INLINE_DISPATCH',  'num',    'extra', false],
  ['xlateReport',    'GEX_XLATE_REPORT',     'xlatereport',   'GEX_XLATE_REPORT',      'num',    'extra', false],
  ['t1Fold',         'GEX_T1_FOLD',          't1fold',        'GEX_T1_FOLD',           'num',    'extra', false],
  // `soname=path,...`: libraries replaced by wasm64 providers (cgen/libprov.rs).
  ['wasmDlls',       'GEX_WASM_DLLS',        'wasmdlls',      'GEX_WASM_DLLS',         'str',    'extra', false],
  // Exports no provider binds, as GEX_PROVIDER_SKIP natively (units built
  // with the same skip).
  ['providerSkip',   'GEX_PROVIDER_SKIP',    'provskip',      'GEX_PROVIDER_SKIP',     'str',    'extra', false],
  // `+name,-name` (fjit.rs `gex_fjit_libs`).
  ['fjitLibs',       'GEX_FJIT_LIBS',        'fjitlibs',      null,                    'str',    'extra', false],
  // Narrate every syscall of every process (inherited by children), or of
  // one pid, or of one program's processes.
  ['strace',         'GEX_STRACE',           'strace',        null,                    'flag',   'opts',  false],
  ['stracePid',      'GEX_STRACE_PID',       'stracepid',     'GEX_STRACE_PID',        'num',    'opts',  false],
  // A parked thread says what it waits on once its wait has lasted N s,
  // 2N, 4N... (waitq.rs `PARK_REPORT_NS`): a hung page's threads.
  ['parkReport',     'GEX_PARK_REPORT',      'parkreport',    'GEX_PARK_REPORT',       'num',    'opts',  false],
  ['straceExe',      'GEX_STRACE_EXE',       'straceexe',     null,                    'str',    'opts',  false],
];

/** A row's value from a string (a URL key's or an environment variable's),
 *  or undefined when it is not set. */
function parse(kind, s) {
  if (s === null || s === undefined) return undefined;
  switch (kind) {
    case 'flag': return s !== '' ? 1 : undefined;
    case 'one': return s === '1' ? 1 : undefined;
    case 'num': case 'num0': return Number(s);
    case 'numor1': return Number(s) || 1;
    default: return s !== '' ? String(s) : undefined;
  }
}

/** What goes into a start message for value `v`: left out when unset or off. */
function carried(kind, v) {
  if (kind === 'num0') return typeof v === 'number' ? v : undefined;
  if (kind === 'flag' || kind === 'one') return v ? 1 : undefined;
  return v || undefined;
}

/** The page's switches from its URL (`get(key)`, null when absent), onto
 *  the globals. */
export function pageOptions(get) {
  for (const [, global, url, , kind] of OPTIONS) {
    const v = parse(kind, get(url));
    if (v !== undefined) globalThis[global] = v;
  }
}

/** The switches set as globals that go `to` a start message's `extra` or
 *  `opts`, by name. */
export function startOptions(to) {
  const out = {};
  for (const [name, global, , , kind, where] of OPTIONS) {
    if (where !== to) continue;
    const v = carried(kind, globalThis[global]);
    if (v !== undefined) out[name] = v;
  }
  return out;
}

/** A Worker's start message `msg`, onto the globals its runtime reads. */
export function workerOptions(msg) {
  for (const [name, global, , , kind, , worker] of OPTIONS) {
    if (!worker) continue;
    const v = carried(kind, msg?.[name]);
    if (v !== undefined) globalThis[global] = v;
  }
}

/** The switches the environment `env` sets that go `to` `extra` or `opts`,
 *  by name (run-node.mjs). */
export function envOptions(env, to) {
  const out = {};
  for (const [name, , , envKey, kind, where] of OPTIONS) {
    if (where !== to || !envKey) continue;
    const v = carried(kind, parse(kind, env[envKey]));
    if (v !== undefined) out[name] = v;
  }
  return out;
}
