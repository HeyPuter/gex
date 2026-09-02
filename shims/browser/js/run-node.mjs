// Run the browser runtime under Node: the same `gex.mjs` a page loads, with
// Node supplying the file reads and a console. If this works, the browser path
// works -- nothing below is Node-specific.
//
//   node shims/browser/js/run-node.mjs [opts] <guest.wasm> <program.elf> [args...]
//
//     --file <guest>=<host>     bundle a host file into the guest's filesystem
//     --dir <guest>=<host>      bundle a whole directory tree
//     --extract <guest>=<host>  write a file back out when the program ends
//     --argv0 <name>            what the guest sees as argv[0]
//     --cwd <dir>               the directory the guest starts in
//     --strace               narrate every syscall to stderr

import { lstatSync, readdirSync, readFileSync, statSync, writeFileSync, writeSync } from 'node:fs';
import { basename, dirname, join } from 'node:path';
import { Worker } from 'node:worker_threads';
import { ProcessHost } from './host.mjs';
// `GEX_ARCH=x86` runs the x86-64 guest: the `.x86`-infixed shim and kernel
// (see shims/browser/build.sh), and `globalThis.GEX_ARCH` for the glue.
const ARCH_INFIX = process.env.GEX_ARCH === 'x86' ? '.x86' : '';
if (process.env.GEX_ARCH === 'x86') globalThis.GEX_ARCH = 'x86';

// The host runs on the main thread; give it the attach-trace flag too so
// thread spawn/adopt timing is visible from the host side, not just the Worker.
if (process.env.GEX_ATTACH_TRACE) globalThis.GEX_ATTACH_TRACE = 1;

// Options come first and are named, so a guest argument that happens to
// contain an `=` -- `lua -e 'local x=1'`, say -- reaches the guest intact.
const files = {};
let strace = false;
let cwd = null;
const extract = [];
let argv0 = null;
const rest = [];
{
  const av = process.argv.slice(2);
  let i = 0;
  for (; i < av.length; i++) {
    if (av[i] === '--file') {
      const spec = av[++i] ?? '';
      const eq = spec.indexOf('=');
      if (eq <= 0) { console.error('--file wants <guest>=<host>'); process.exit(2); }
      files[spec.slice(0, eq)] = readFileSync(spec.slice(eq + 1));
    } else if (av[i] === '--dir') {
      // A whole directory tree, which is what a program that reads headers or
      // data files needs and what a page would bundle up front.
      const spec = av[++i] ?? '';
      const eq = spec.indexOf('=');
      if (eq <= 0) { console.error('--dir wants <guest>=<host>'); process.exit(2); }
      const [guestRoot, hostRoot] = [spec.slice(0, eq), spec.slice(eq + 1)];
      const walk = (guest, host) => {
        // Unreadable directories are ordinary in a rootfs (mode 0700, root),
        // and one must not take the whole run down.
        let names;
        try {
          names = readdirSync(host);
        } catch {
          return;
        }
        for (const name of names) {
          const h = `${host}/${name}`, g = `${guest}/${name}`;
          // A rootfs has dangling symlinks (Debian's /usr/bin/ld.so), and
          // `stat` follows links so it throws on them. Skipping matches what a
          // guest sees: the link resolves to nothing there too.
          let st;
          try {
            st = statSync(h);
          } catch {
            continue;
          }
          // A directory *symlink* is not descended into: Debian's
          // `/usr/bin/X11 -> .` reads as a directory, so this walked into it
          // forever until a too-long path blew up. The target is already
          // walked under its real name.
          if (st.isDirectory()) {
            let link;
            try {
              link = lstatSync(h);
            } catch {
              continue;
            }
            if (!link.isSymbolicLink()) walk(g, h);
          } else {
            try {
              files[g] = readFileSync(h);
            } catch {
              // Unreadable, or a name the host rejects. One file is not the
              // run.
            }
          }
        }
      };
      walk(guestRoot.replace(/\/$/, ''), hostRoot.replace(/\/$/, ''));
    } else if (av[i] === '--extract') {
      const spec = av[++i] ?? '';
      const eq = spec.indexOf('=');
      if (eq <= 0) { console.error('--extract wants <guest>=<host>'); process.exit(2); }
      extract.push([spec.slice(0, eq), spec.slice(eq + 1)]);
    } else if (av[i] === '--cwd') cwd = av[++i];
    else if (av[i] === '--strace') strace = true;
    else if (av[i] === '--argv0') argv0 = av[++i];
    else break;
  }
  rest.push(...av.slice(i));
}

const [guestPath, elfPath, ...guestArgs] = rest;
if (!guestPath || !elfPath) {
  console.error('usage: run-node.mjs [opts] <guest.wasm> <program.elf> [args...]');
  process.exit(2);
}

// Everything on stdin up front -- the browser host cannot block for more
// later. Nothing on a terminal, so an interactive run does not wait forever.
function readStdin() {
  try {
    return process.stdin.isTTY ? null : readFileSync(0);
  } catch {
    return null;
  }
}

// The same file the page fetches -- `shims/browser/build.sh` installs it here,
// so the harness tests what ships rather than a separately-built copy.
//
// `GEX_SHARED=1` picks the shared-memory pair instead: a memory import's
// `shared` flag is part of its type, so the shim, seed, kernel and every
// closure module must be the shared ones together. That is the configuration
// the page ships and the one a fork breaks in.
const SHARED = !!process.env.GEX_SHARED;
const shimWasm = readFileSync(
  new URL(
    SHARED ? `../../../web/public/gex/gex_browser${ARCH_INFIX}.shared.wasm`
           : `../../../web/public/gex/gex_browser${ARCH_INFIX}.wasm`,
    import.meta.url,
  ),
);

// The global kernel: the process table, filesystem and pipes every process
// shares, in a module every Worker instantiates against one shared memory.
// Optional -- a single-process run needs none of it, but a program that forks
// gets as far as the fork and no further without it.
let kernel = null;
try {
  const path = new URL(
    SHARED ? `../../../web/public/gex/gex_kernel${ARCH_INFIX}.wasm`
           : `../../../web/public/gex/gex_kernel${ARCH_INFIX}.plain.wasm`,
    import.meta.url,
  );
  kernel = {
    module: await WebAssembly.compile(readFileSync(path)),
    // Four gigabytes, all a wasm32 memory can be. The rootfs lives here (the
    // filesystem belongs to the kernel), so this ceiling is the largest rootfs
    // a run can have -- at one gigabyte a full Debian arm64 image did not fit
    // (#91). `maximum` reserves address space, not pages, so raising it is free.
    memory: new WebAssembly.Memory({ initial: 2048, maximum: 65536, shared: true }),
  };
} catch {
  // Not built. `shims/kernel/build.sh` makes it; a run that never forks will
  // not notice, and one that does will say so when it tries.
}

// Everything past here is the host -- `host.mjs`, the same file the page uses,
// with a Node `Worker` in place of a Web one. Pid 1 runs in a Worker, not on
// the main thread: `k_park` is `Atomics.wait`, and a guest parking on the main
// thread would stop the loop that starts its children (`bash -c 'echo $(echo
// hi)'` hung this way). No guest is on the thread the host runs on.
const guestWasm = readFileSync(guestPath);
const elf = readFileSync(elfPath);
const args = [argv0 ?? elfPath, ...guestArgs];

/// Read a `.pins` manifest into `{ path: { base, span } }`.
///
/// The same file `gexc run --pin` takes, and the same format the closure
/// build writes: a hex base, a space, a guest path.
function readPins() {
  const path = process.env.GEX_PINS;
  if (!path) return undefined;
  const out = {};
  for (const line of readFileSync(path, 'utf8').split('\n')) {
    // `base span path`: the span stops the placer handing out an address
    // inside a pinned object.
    const f = line.split(' ');
    if (f.length < 3) continue;
    const base = Number(f[0]);
    const span = Number(f[1]);
    if (!Number.isFinite(base) || base <= 0) continue;
    out[f.slice(2).join(' ').trim()] = { base, span: Number.isFinite(span) ? span : 0 };
  }
  return out;
}

/// Every library module of a closure build, in build order (the order `gexc`
/// allocated slots in; the modules themselves are independent).
function readClosure() {
  const stem = process.env.GEX_CLOSURE;
  if (!stem) return undefined;
  const dir = dirname(stem);
  const base = basename(stem);
  const names = readdirSync(dir)
    // Not the image module itself: that is the guest, not a library.
    .filter((n) => n.startsWith(base + '.') && n.endsWith('.wasm') && n !== base + '.wasm')
    .sort();
  return names.map((n) => readFileSync(join(dir, n)));
}

const host = new ProcessHost({
  makeWorker: () => new Worker(new URL('./worker.mjs', import.meta.url),
    // GEX_CPU_PROF=<dir>: write a V8 CPU profile per Worker, to see where the
    // guest's time actually goes (which translated library functions, JS
    // overhead, GC). Off by default -- it slows the run and writes a file each.
    process.env.GEX_CPU_PROF
      ? { execArgv: ['--cpu-prof', '--cpu-prof-dir', process.env.GEX_CPU_PROF, '--cpu-prof-interval', '2000'] }
      : undefined),
  shim: shimWasm,
  guest: guestWasm,
  kernel,
  // Merged into every process's start message, because on a real machine
  // every process gets the same of all of this.
  extra: {
    // Bytes, not a string: a command line is a pipe, and `gzip -c` writes a
    // deflate stream that a UTF-8 round trip would fill with U+FFFD.
    rawOutput: true,
    // Which memory every process gets; must match the artefacts chosen above.
    shared: SHARED,
    // GEX_CLOSURE names the stem a closure build wrote; each
    // `<stem>.NN.<name>.wasm` is one prebuilt library. In `extra` so a forked
    // child gets it too: a child's own dispatch tables start empty, so it must
    // instantiate the modules itself.
    closure: readClosure(),
    // GEX_NETWORK gives the guest a network of real TCP sockets. Inherited:
    // apt fetches in a forked helper, so the one process needing a network was
    // the only one without one when this was not carried.
    network: !!process.env.GEX_NETWORK,
    // GEX_WL_STUB: run the shim's gex-wayland compositor with a headless present
    // (no canvas), so a native-wayland guest connects and inits under node --
    // for reproducing the browser-only native-wayland thread deadlock with node
    // tooling. Every process gets it (gtk3-demo is a forked child of the shell).
    wlStub: !!process.env.GEX_WL_STUB,
    // GEX_WL_TRACE: narrate the wayland protocol (the wasm shim has no env, so
    // the host flips the switch via gex_set_wl_trace in the worker).
    wlTrace: process.env.GEX_WL_TRACE ? 1 : undefined,
    attachTrace: !!process.env.GEX_ATTACH_TRACE,
    // GEX_LAZY_THREAD: spawned threads register the process's modules lazily
    // (instance on first branch) instead of installing them all up front.
    lazyThread: !!process.env.GEX_LAZY_THREAD,
    debugExit: !!process.env.GEX_DEBUG_EXIT,
    // GEX_HEARTBEAT: say where the guest is every N safepoints and every 200
    // dispatch misses, so a slow run can be told from a stuck one.
    heartbeat: process.env.GEX_HEARTBEAT ? Number(process.env.GEX_HEARTBEAT) : 0,
    // GEX_RECOMPILE_OBJECTS statically recompiles each shared object once it
    // proves hot -- the feature this project exists for. The value is a cap in
    // KiB of mapped text; 8 MB is the largest that works (Mesa's rasteriser
    // above it brings the run down inside ld.so). See #73.
    recompileObjects: process.env.GEX_RECOMPILE_OBJECTS
      ? Number(process.env.GEX_RECOMPILE_OBJECTS) || 8192
      : 0,
    // GEX_LINK keeps the address space the guest's `ld.so` produced and hands
    // it to the next process of the same program, relocations already applied.
    link: Number(process.env.GEX_LINK || 0),
    // GEX_TRACE is the syscall log, large enough to want a question already
    // narrowed to a few seconds of guest time.
    trace: !!process.env.GEX_TRACE,
    traceMisses: !!process.env.GEX_TRACE_MISSES,
    // GEX_TRACE_BLOCKS: the per-block register trace, as natively.
    traceBlocks: !!process.env.GEX_TRACE_BLOCKS,
    watchMem: process.env.GEX_WATCH_MEM || undefined,
    // GEX_CHECK_SP checks `sp` at the top of every block: a corrupt `sp` is
    // invisible until dereferenced, so this names the block that arrives with
    // the bad value, not the one that dies on it. See #73.
    checkSp: !!process.env.GEX_CHECK_SP,
    // GEX_COHERENT_LOADS emits guest scalar loads/stores as atomic-when-aligned
    // so shared reads stay coherent under true parallelism (V8 can't hoist a
    // plain wasm load across a guest fence). Tests whether the GTK startup race
    // is plain-load coherence at the PLAN 5.8 boundary. Default off.
    coherentLoads: !!process.env.GEX_COHERENT_LOADS,
    // GEX_SP_GUARD: at each safepoint, catch a worker thread whose SP has strayed
    // into the MAIN thread's stack (it would smash a canary the main thread only
    // detects later -- the complex-Qt/designer stack smash). Loud when it fires.
    spGuard: !!process.env.GEX_SP_GUARD,
    // GEX_TRAP_PC=0xADDR: plant a diagnostic breakpoint at that guest pc that
    // dumps the live registers (x30 = caller) and continues. Aimed at
    // __stack_chk_fail to name the smashed function.
    trapPc: process.env.GEX_TRAP_PC || 0,
    // GEX_PREGROW: grow guest memory to a large floor up front so runtime
    // memory.grow stops during startup -- probes the stale-bound-after-grow race.
    pregrow: !!process.env.GEX_PREGROW,
    // GEX_HOT dumps the guest's hottest back edges on exit -- the browser's
    // answer to `--sample`, since a host profile is all `wasm-function[N]`.
    hot: !!process.env.GEX_HOT,
    // GEX_NO_INTRINSICS translates glibc's own memcpy/memcmp/memset instead of
    // serving them, the only way to price the substitution.
    noIntrinsics: !!process.env.GEX_NO_INTRINSICS,
    // GEX_INTRINSIC_MASK bisects which family corrupts memory: bit0=memcpy,
    // bit1=memset, bit2=memcmp. e.g. 1 = memcpy only, 2 = memset only.
    intrinsicMask: process.env.GEX_INTRINSIC_MASK
      ? parseInt(process.env.GEX_INTRINSIC_MASK, 10) >>> 0
      : undefined,
    // GEX_FMA_COUNT: how many fused multiply-adds crossed out of the guest
    // module. wasm has no FMA, so each is a call into the shim's.
    fmaCount: !!process.env.GEX_FMA_COUNT,
    // Anything the program leaves behind, read out by the Worker that owns its
    // address space -- this thread has no view of it.
    extract: extract.map(([guest]) => guest),
  },
  onOutput: (fd, text, pid, m) => {
    // Through `process.stdout`, not `writeSync(1, ...)`: this runner is itself
    // sometimes inside a Worker, whose fd 1 is the process's fd 1 -- writing it
    // directly interleaved with the parent's output out of order.
    const bytes = m?.bytes ? Buffer.from(m.bytes) : text;
    const s = fd === 1 ? process.stdout : fd === 2 ? process.stderr : null;
    // Anything above 2 is a real descriptor and still goes to the fd.
    if (s) s.write(bytes);
    else writeSync(fd, typeof bytes === 'string' ? Buffer.from(bytes) : bytes);
  },
  onExit: (m, pid) => {
    // How much of the run was translating vs. running translated code. Per
    // process, because a program that forks pays it again.
    if (process.env.GEX_TIME_JIT) {
      process.stderr.write(
        // `guestMillis` is the whole time inside the guest (load, install,
        // run); `millis` is only the translator.
        `[gex] pid ${pid}: ${m.guestMillis ?? '?'}ms total, ${m.millis}ms translating ${m.blocks} blocks` +
          (m.installs ? `, ${m.installMs}ms installing ${m.installs} modules` +
            ` (${m.compileMs}ms compile, ${m.instantiateMs}ms instantiate, ${m.growMs}ms grow)` : '') +
          `, worker start ${(m.shimCompileMs ?? 0) + (m.shimInstMs ?? 0) + (m.guestInstMs ?? 0)}ms` + ` (shim ${m.shimCompileMs}c/${m.shimInstMs}i, guest ${m.guestInstMs}i)` +
          // Where the shared-slot lookup went, to say why a forked and exec'd
          // child lifts its parent's libraries again.
          (m.reuse
            ? `, reuse have=${m.reuse[0]} hit=${m.reuse[1]} miss=${m.reuse[2]} noident=${m.reuse[3]}` +
              ` served=${m.reuse[4]} stored=${m.reuse[5]} refused=${m.reuse[6]} unclaimed=${m.reuse[7]} allmine=${m.reuse[8]}`
            : '') +
          '\n',
      );
    }
    if (pid !== 1) {
      if (process.env.GEX_DEBUG_EXIT) {
        const mem = m.memoryMib === undefined ? '' : `, memory ${m.memoryMib} MiB`;
        process.stderr.write(`[gex] process ${pid} exited ${m.status}${mem}\n`);
      }
      return;
    }
    // GEX_TIME_GUEST prints how long the guest itself ran, not how long the
    // process took: a third of the total is V8 compiling the module, which
    // buries a codegen A/B. `tests/ab.sh --metric` reads this line.
    if (process.env.GEX_TIME_GUEST) {
      process.stderr.write(`[gex] guest ran in ${m.guestMillis}ms\n`);
    }
    // What this process's memory grew to; a run costs this times its process count.
    if (process.env.GEX_DEBUG_EXIT && m.memoryMib !== undefined) {
      process.stderr.write(`[gex] memory ${m.memoryMib} MiB\n`);
    }
    if (process.env.GEX_HOT) {
      // An empty answer is a question about the run, not a crash.
      if (!m.hot?.length) {
        process.stderr.write(
          '[hot] nothing counted. The shim may predate `gex_set_hot`; rebuild it.\n',
        );
      }
      for (const line of m.hot ?? []) process.stderr.write(`[hot] ${line}\n`);
    }
    if (process.env.GEX_FMA_COUNT && m.fmaCalls !== undefined) {
      process.stderr.write(`[gex] fma calls: ${m.fmaCalls}\n`);
    }
    const got = new Map(m.extracted ?? []);
    for (const [guest, hostPath] of extract) {
      const bytes = got.get(guest);
      if (bytes) writeFileSync(hostPath, Buffer.from(bytes));
    }
  },
  onMessage: (m, pid) => {
    // Memory before the guest runs an instruction: whether the cost is placing
    // the image or running it.
    if (m?.kind === 'loaded' && pid === 1 && process.env.GEX_DEBUG_EXIT) {
      process.stderr.write(`[gex] memory after load: ${m.memoryMib.toFixed(0)} MiB\n`);
    }
  },
});

let status;
try {
  status = await host.start({
    elf,
    files,
    args,
    opts: {
      strace,
      // The debugging switches, in the run's own options as well as the boot
      // message: `opts` is what a fork record carries to a child's Worker.
      traceBlocks: !!process.env.GEX_TRACE_BLOCKS,
      traceMisses: !!process.env.GEX_TRACE_MISSES,
      checkSp: !!process.env.GEX_CHECK_SP,
      watchMem: process.env.GEX_WATCH_MEM || undefined,
      noIntrinsics: !!process.env.GEX_NO_INTRINSICS,
      intrinsicMask: process.env.GEX_INTRINSIC_MASK ? Number(process.env.GEX_INTRINSIC_MASK) : undefined,
      // The suite compares this run against QEMU on the same machine, and QEMU
      // reports the real user.
      uid: process.getuid?.(),
      gid: process.getgid?.(),
      stdin: readStdin(),
      cwd,
      // GEX_PINS names a layout written by `gexc build --closure`. Every
      // object in it is placed exactly where the build assumed, which is
      // what makes a module built against those addresses correct -- and it
      // goes into the *global* placement table too, so every process of the
      // run agrees rather than only the first.
      pins: readPins(),
    },
  });
} catch (e) {
  process.stderr.write(`gex: ${e?.message ?? e}\n`);
  status = 1;
}
// `process.exitCode`, not `process.exit`: the guest's output goes through
// `process.stdout`/`process.stderr` (async when a pipe), and `process.exit`
// does not wait for them to drain. Setting the code lets node finish the writes.
process.exitCode = status;
