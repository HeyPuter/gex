// Run the browser runtime under Node.
//
// The same `gex.mjs` a page would load; Node only supplies the file reads and
// a console. If this works, the browser path works, because nothing below is
// Node-specific.
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
        // Unreadable directories are ordinary in a root filesystem --
        // /usr/lib/ssl/private is mode 0700 and owned by root -- and one of
        // them must not take the whole run down.
        let names;
        try {
          names = readdirSync(host);
        } catch {
          return;
        }
        for (const name of names) {
          const h = `${host}/${name}`, g = `${guest}/${name}`;
          // A real root filesystem has dangling symlinks in it -- Debian's
          // has /usr/bin/ld.so pointing at a package that is not installed --
          // and `stat` follows links, so it throws on them. Skipping is what
          // a guest sees anyway: the link resolves to nothing there too.
          let st;
          try {
            st = statSync(h);
          } catch {
            continue;
          }
          // A directory *symlink* is not descended into. Debian ships
          // `/usr/bin/X11 -> .`, and `stat` says that is a directory, so this
          // walked into it and re-read the whole of `/usr/bin` one level
          // deeper, for ever: the path grew `X11/` per turn until an
          // uncaught `readFileSync` blew up on a name too long, and took the
          // whole apt run with it. Following the link buys nothing either --
          // whatever it points at is already being walked under its real
          // name.
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

// Everything on stdin, up front -- the browser host has no way to block for
// more later. Nothing on a terminal, so an interactive run is not left
// waiting for a pipe that will never close.
function readStdin() {
  try {
    return process.stdin.isTTY ? null : readFileSync(0);
  } catch {
    return null;
  }
}

// The same file the page fetches, deliberately.
//
// This used to read `target/wasm32-unknown-unknown/release/gex_browser.wasm`
// while the page fetched `/gex/gex_browser.wasm` out of `web/public`, and
// only `web/build-assets.sh` connected the two. So this harness could pass
// against a shim the demo was not running -- and it did, for hours, while
// every boot of the page died with "Cannot set properties of undefined"
// because its copy had been built without `--export=__stack_pointer`.
//
// `shims/browser/build.sh` installs here now, after checking the glue. One
// artefact, one build, and a harness that tests what ships.
const shimWasm = readFileSync(
  new URL('../../../web/public/gex/gex_browser.wasm', import.meta.url),
);

// The global kernel: the process table, the filesystem and the pipes every
// process shares, in a module every Worker instantiates against one shared
// memory. Optional, because a run of one process needs none of it -- but
// without it a program that forks gets as far as the fork and no further.
let kernel = null;
try {
  const path = new URL('../../../web/public/gex/gex_kernel.plain.wasm', import.meta.url);
  kernel = {
    module: await WebAssembly.compile(readFileSync(path)),
    // Four gigabytes, which is all a wasm32 memory can be.
    //
    // The rootfs lives in here -- the filesystem belongs to the kernel, and
    // every file the host bundles is copied into this memory -- so this
    // ceiling is the largest rootfs a run can have. At one gigabyte a
    // complete Debian arm64 image did not fit, and the failure was a bare
    // `unreachable` out of `rust_oom` with nothing said about why. That is
    // #91, and it is what stops `apt-get update` running here rather than
    // anything about apt.
    //
    // Raising it costs nothing: `maximum` reserves address space, not pages.
    // The memory still starts at 128 MiB and grows as files arrive.
    memory: new WebAssembly.Memory({ initial: 2048, maximum: 65536, shared: true }),
  };
} catch {
  // Not built. `shims/kernel/build.sh` makes it; a run that never forks will
  // not notice, and one that does will say so when it tries.
}

// Everything past here is the host, and it is `host.mjs` -- the same file the
// page uses, with a Node `Worker` in place of a Web one.
//
// It used to be a copy: this file ran pid 1 itself, on the main thread, and
// wired children up one at a time. That was not only duplication. `k_park` is
// `Atomics.wait`, and a guest that parks on the main thread stops the event
// loop -- so the loop that starts children, delivers their output and answers
// their forks stopped exactly when a child was most needed. `bash -c 'echo
// $(echo hi)'` hung: bash forked and read the pipe, and the process that was
// going to write it had not been created yet.
//
// Pid 1 in a Worker removes the case rather than working around it. Nothing
// the guest system needs happens on the thread a guest can block, because no
// guest is on it. It also makes this runner's claim true -- if a program works
// here the browser path works -- which it was not while the two hosts ran
// different process models.
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
    // `base span path`: three fields, and the span matters as much as the
    // base -- it is what stops the automatic placer handing out an address
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

/// Every library module of a closure build, in build order.
///
/// Order matters only in that it is the order `gexc` allocated slots in, and
/// installing them in it keeps the trace readable; the modules themselves are
/// independent.
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
  makeWorker: () => new Worker(new URL('./worker.mjs', import.meta.url)),
  shim: shimWasm,
  guest: guestWasm,
  kernel,
  // Merged into every process's start message, because on a real machine
  // every process gets the same of all of this.
  extra: {
    // The bytes, not a string. A page has a terminal and wants text; a
    // command line is a pipe, and `gzip -c` writes a deflate stream through
    // it that a UTF-8 round trip would fill with U+FFFD.
    rawOutput: true,
    // GEX_CLOSURE names the stem a closure build wrote. Every
    // `<stem>.NN.<name>.wasm` beside it is one library, translated ahead of
    // time; the image module is `<stem>.wasm` and is passed as the guest.
    //
    // In `extra` rather than `opts`, so a forked child gets it too. A child
    // adopts its parent's memory and with it the dispatch *directory*, but
    // its own dispatch tables belong to its own guest instance and start
    // empty -- so it has to instantiate the modules itself to have anything
    // for the directory to point at.
    closure: readClosure(),
    // GEX_NETWORK gives the guest a network made of real TCP sockets, so the
    // paths above Wisp -- apt's helper, a fetch, a parse -- are exercised here
    // rather than only in a browser with a proxy configured. Per process, and
    // inherited: apt does its fetching in a forked helper, and when that was
    // not carried the one process that needed a network was the only one
    // without one.
    network: !!process.env.GEX_NETWORK,
    debugExit: !!process.env.GEX_DEBUG_EXIT,
    // GEX_HEARTBEAT asks the shim to say where the guest is every N safepoints
    // and every 200 dispatch misses. Nothing else runs while translated code
    // does, so without it a run that is merely slow cannot be told from one
    // that is stuck.
    heartbeat: process.env.GEX_HEARTBEAT ? Number(process.env.GEX_HEARTBEAT) : 0,
    // GEX_RECOMPILE_OBJECTS makes the runtime statically recompile each shared
    // object once it proves hot, instead of meeting it a dispatch miss at a
    // time. That is what the native host does with `--recompile-objects`, and
    // it is the feature this project exists for.
    //
    // The value is a cap in KiB of mapped text. Eight megabytes is not a
    // principled number -- it is the largest that works: every one of the 57
    // libraries in the graphics rootfs is under it, and the one object above
    // it, Mesa's software rasteriser at 15 MB of text and 471,205 blocks,
    // brings the run down inside ld.so shortly after it is mapped. See #73.
    recompileObjects: process.env.GEX_RECOMPILE_OBJECTS
      ? Number(process.env.GEX_RECOMPILE_OBJECTS) || 8192
      : 0,
    // GEX_TRACE is the syscall log. Large -- a run that translates Mesa
    // produces hundreds of megabytes of it -- so it is for a question already
    // narrowed to a few seconds of guest time, like which process sent the
    // signal that ended a run.
    // GEX_LINK keeps the address space the guest's `ld.so` produced and
    // hands it to the next process of the same program, which then starts at
    // its own entry with every relocation already applied.
    link: !!process.env.GEX_LINK,
    trace: !!process.env.GEX_TRACE,
    traceMisses: !!process.env.GEX_TRACE_MISSES,
    // GEX_CHECK_SP puts a stack-pointer check at the top of every block the
    // shim translates from here on. A corrupt `sp` is invisible until
    // something dereferences it, and by then the guest is deep inside an
    // innocent function -- #73 ends in an ordinary prologue faulting on its
    // own first store. This names the block that arrives with the bad value
    // instead of the one that dies on it.
    checkSp: !!process.env.GEX_CHECK_SP,
    // GEX_HOT dumps the guest's hottest back edges when the run ends, which is
    // the browser's answer to the native host's `--sample`. A block-per-
    // function translation makes a profile of the *host* nearly useless -- it
    // is all `wasm-function[N]` across thousands of modules -- so the question
    // "which guest code is hot" needs asking of the guest.
    hot: !!process.env.GEX_HOT,
    // GEX_NO_INTRINSICS translates glibc's own memcpy/memcmp/memset instead of
    // serving them, which is the only way to price the substitution.
    noIntrinsics: !!process.env.GEX_NO_INTRINSICS,
    // GEX_FMA_COUNT reports how many fused multiply-adds crossed out of the
    // guest module. wasm has no FMA, so each one is a call into the shim's.
    fmaCount: !!process.env.GEX_FMA_COUNT,
    // Anything the program is asked to leave behind, read out of its address
    // space by the Worker that owns it -- this thread has no view of it.
    extract: extract.map(([guest]) => guest),
  },
  onOutput: (fd, text, pid, m) => {
    // Through `process.stdout` rather than `writeSync(1, ...)`: this runner is
    // itself sometimes inside a Worker, and a Worker's fd 1 is the *process's*
    // fd 1 -- output went straight past the piped stream and interleaved with
    // the parent's, out of order and unfilterable.
    const bytes = m?.bytes ? Buffer.from(m.bytes) : text;
    const s = fd === 1 ? process.stdout : fd === 2 ? process.stderr : null;
    // Anything above 2 is a real descriptor and still goes to the fd.
    if (s) s.write(bytes);
    else writeSync(fd, typeof bytes === 'string' ? Buffer.from(bytes) : bytes);
  },
  onExit: (m, pid) => {
    // How much of the run was translating, as opposed to running what was
    // translated. Every process separately, because a program that forks
    // pays it again -- which is the whole question about whether translating
    // ahead of time is worth anything.
    if (process.env.GEX_TIME_JIT) {
      process.stderr.write(
        // `guestMillis` is the whole time this Worker was inside the guest --
        // loading the image, installing modules, and running. Beside it,
        // `millis` is only the translator. The two together are what say
        // whether a slow run is translation or everything else, and until now
        // the second was computed, posted and thrown away.
        `[gex] pid ${pid}: ${m.guestMillis ?? '?'}ms total, ${m.millis}ms translating ${m.blocks} blocks` +
          (m.installs ? `, ${m.installMs}ms installing ${m.installs} modules` +
            ` (${m.compileMs}ms compile, ${m.instantiateMs}ms instantiate, ${m.growMs}ms grow)` : '') +
          `, worker start ${(m.shimCompileMs ?? 0) + (m.shimInstMs ?? 0) + (m.guestInstMs ?? 0)}ms` + ` (shim ${m.shimCompileMs}c/${m.shimInstMs}i, guest ${m.guestInstMs}i)` +
          // Where the shared-slot lookup went, when the shim reports it:
          // pages this process already had, pages the identity map knew,
          // pages it did not, and pages with no identity at all. The
          // question these answer is why a forked and exec'd child lifts
          // its parent's libraries again.
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
    // GEX_TIME_GUEST prints how long the guest itself ran, which is not the
    // same question as how long this process took.
    //
    // A run of statically recompiled perl is about nine seconds of wall clock,
    // of which two to four is V8 compiling a seventeen-megabyte module before
    // the guest executes an instruction. Comparing two *codegen* changes
    // through that total means comparing a quantity where a third of the
    // signal is module size and V8's compiler, and on a machine with a noisy
    // neighbour it buries an effect the paired A/B could otherwise resolve.
    // `tests/ab.sh --metric` reads this line instead of timing the process.
    if (process.env.GEX_TIME_GUEST) {
      process.stderr.write(`[gex] guest ran in ${m.guestMillis}ms\n`);
    }
    // What this process's memory actually grew to. One memory per process, so
    // this is the number multiplied by however many processes a run has.
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
    // Before the guest has executed an instruction. A process's memory is what
    // decides how many of them a browser will hold, so it matters whether the
    // cost is placing the image or running it.
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
// `process.exitCode`, not `process.exit`. The guest's output goes through
// `process.stdout`/`process.stderr` -- it has to, so that this runner works
// inside a Worker, whose fd 1 is the *process's* -- and those are asynchronous
// when they are a pipe. `process.exit` does not wait for them, so whatever had
// not drained was thrown away: `apt update` appeared to stop after its first
// download, with the rest of its output and its error message both lost, and
// the run looked like a hang that had exited 0.
//
// Setting the code instead lets node finish the writes and leave on its own.
process.exitCode = status;
