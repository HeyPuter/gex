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

import { readdirSync, readFileSync, statSync, writeFileSync, writeSync } from 'node:fs';
import { Gex, Env } from './gex.mjs';

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
          if (st.isDirectory()) walk(g, h);
          else files[g] = readFileSync(h);
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

class NodeEnv extends Env {
  /**
   * The guest's stdout and stderr, on this process's own streams.
   *
   * Through `process.stdout` rather than `writeSync(1, ...)` because this
   * runner also runs inside a Worker, and a Worker's fd 1 is the *process's*
   * fd 1 -- the guest's output went straight past the Worker's piped stream
   * and interleaved with the parent's, out of order and unfilterable. Any
   * other descriptor is a real file, and still goes to the fd.
   *
   * Copied, because `writeSync` was synchronous and this is not: `bytes` is a
   * view into the guest's memory, and that memory can be written again -- or
   * grown, which detaches every view of it -- long before the stream flushes.
   */
  write(fd, bytes) {
    const s = fd === 1 ? process.stdout : fd === 2 ? process.stderr : null;
    if (s) s.write(Buffer.from(bytes));
    else writeSync(fd, bytes);
  }
  now() { return BigInt(Date.now()) * 1000000n; }
}

const shimWasm = readFileSync(
  new URL('../../../target/wasm32-unknown-unknown/release/gex_browser.wasm', import.meta.url),
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
    memory: new WebAssembly.Memory({ initial: 2048, maximum: 16384, shared: true }),
  };
} catch {
  // Not built. `shims/kernel/build.sh` makes it; a run that never forks will
  // not notice, and one that does will say so when it tries.
}

/**
 * Start a child the guest forked, in a `worker_thread` of its own.
 *
 * The same `worker.mjs` a page uses. What is different here is only that the
 * *first* process is not in a Worker -- this file is the main thread and runs
 * it directly -- so children are wired up one at a time rather than through
 * `host.mjs`.
 */
// The global kernel's per-Worker stacks: every Worker's instance has a stack
// pointer of its own, and wasm-ld starts all of them at the same address in
// the memory they share. This process holds the first; every child gets a
// slice of the array the kernel reserves.
let kernelStacks = null;
let kernelStacksTaken = 0;

async function spawnChild(child, guestWasm) {
  const { Worker } = await import('node:worker_threads');
  // Not unref'd: a child outlives the parent, and this process has to stay
  // up while it does. `park-across-switch` is exactly that shape -- the
  // parent returns immediately and the whole point is what the child does
  // afterwards -- and with an unref'd Worker node left as soon as the parent
  // was done and the child's output never appeared.
  const w = new Worker(new URL('./worker.mjs', import.meta.url));
  w.on('message', (m) => {
    if (m.kind === 'ready') {
      w.postMessage({
        kind: 'adopt',
        child,
        shim: shimWasm,
        guest: guestWasm,
        kernel,
        kernelStack: kernelStacks
          ? kernelStacks.area + ++kernelStacksTaken * kernelStacks.size
          : undefined,
        debugExit: !!process.env.GEX_DEBUG_EXIT,
      });
    } else if (m.kind === 'output') {
      (m.fd === 2 ? process.stderr : process.stdout).write(m.text);
    } else if (m.kind === 'fork') {
      spawnChild(m.child, guestWasm);
    } else if (m.kind === 'stdin') {
      // A child inherits its descriptors; anything it reads from the terminal
      // it reads through one of those, not through this.
      w.postMessage({ kind: 'eof' });
    } else if (m.kind === 'exit' || m.kind === 'threw') {
      if (m.kind === 'threw') process.stderr.write(`gex: process ${child.pid}: ${m.message}\n`);
      else if (process.env.GEX_DEBUG_EXIT) {
        process.stderr.write(`[gex] process ${child.pid} exited ${m.status}\n`);
      }
      w.terminate();
    }
  });
  w.on('error', (e) => {
    process.stderr.write(`gex: process ${child.pid} failed: ${e.message}\n`);
  });
}

const guestWasm = readFileSync(guestPath);
class ForkingEnv extends NodeEnv {
  async spawnProcess(child) {
    await spawnChild(child, guestWasm);
  }
}
const gex = new Gex(new ForkingEnv(), { kernel });
const args = [argv0 ?? elfPath, ...guestArgs];
const entry = await gex.load(shimWasm, guestWasm, readFileSync(elfPath), files, args, {
  strace,
  // The suite compares this run against QEMU on the same machine, and QEMU
  // reports the real user.
  uid: process.getuid?.(),
  gid: process.getgid?.(),
  stdin: readStdin(),
  cwd,
});
// This process is the first Worker, so it is the one that can safely ask
// where the kernel's per-Worker stacks are. Every child gets a slice.
kernelStacks = gex.kernelStacks?.() ?? null;
// GEX_NETWORK gives the guest a network made of real TCP sockets, so the
// paths above Wisp -- apt's helper, a fetch, a parse -- can be exercised here
// rather than only in a browser with a proxy configured.
if (process.env.GEX_NETWORK) {
  const { NodeNetwork } = await import('./net-node.mjs');
  gex.useNetwork(new NodeNetwork());
}
// GEX_HEARTBEAT asks the shim to say where the guest is every N safepoints
// and every 200 dispatch misses. Nothing else runs while translated code
// does, so without it a run that is merely slow cannot be told from one that
// is stuck -- and this is the fast lane where that question gets asked.
if (process.env.GEX_HEARTBEAT) {
  gex.shim.exports.gex_set_heartbeat?.(BigInt(process.env.GEX_HEARTBEAT));
}
// GEX_RECOMPILE_OBJECTS makes the runtime statically recompile each shared
// object as the guest maps it, instead of meeting it a dispatch miss at a
// time. That is what the native host does with `--recompile-objects`, and
// what a page has never had -- there is no ahead-of-time step in a browser,
// so every block used to arrive through a miss.
// GEX_TRACE is the syscall log. Large -- a run that translates Mesa produces
// hundreds of megabytes of it -- so it is for a question already narrowed to
// a few seconds of guest time, like which process sent the signal that ended
// a run.
if (process.env.GEX_DEBUG_EXIT) globalThis.GEX_DEBUG_EXIT = 1;
if (process.env.GEX_TRACE) {
  gex.shim.exports.gex_set_trace?.(1);
}
if (process.env.GEX_TRACE_MISSES) {
  gex.shim.exports.gex_set_trace_misses?.(1);
}
// GEX_CHECK_SP puts a stack-pointer check at the top of every block the shim
// translates from here on. A corrupt `sp` is invisible until something
// dereferences it, and by then the guest is deep inside an innocent function
// -- #73 ends in an ordinary prologue faulting on its own first store. This
// names the block that arrives with the bad value instead of the one that
// dies on it.
if (process.env.GEX_CHECK_SP) {
  gex.shim.exports.gex_set_check_sp?.(1);
}
if (process.env.GEX_RECOMPILE_OBJECTS) {
  // The value is a cap in KiB of mapped text. Eight megabytes is not a
  // principled number -- it is the largest that works: every one of the 57
  // libraries in the graphics rootfs is under it, and the one object above
  // it, Mesa's software rasteriser at 15 MB of text and 471,205 blocks,
  // brings the run down inside ld.so shortly after it is mapped. See #73.
  gex.shim.exports.gex_set_recompile_objects?.(
    Number(process.env.GEX_RECOMPILE_OBJECTS) || 8192,
  );
}
// GEX_HOT dumps the guest's hottest back edges when the run ends, which is
// the browser's answer to the native host's `--sample`. A block-per-function
// translation makes a profile of the *host* nearly useless -- it is all
// `wasm-function[N]` across thousands of modules -- so the question "which
// guest code is hot" needs asking of the guest.
const status = await gex.run(entry);
if (process.env.GEX_HOT && gex.shim.exports.gex_hot_blocks) {
  const e = gex.shim.exports;
  const at = e.gex_alloc(4096) >>> 0;
  const n = e.gex_hot_blocks(at, 4096) >>> 0;
  const text = new TextDecoder().decode(new Uint8Array(gex.memory.buffer, at, n));
  for (const line of text.trim().split('\n')) {
    const [pc, count] = line.split(' ');
    // Named where it can be: an address alone is a library and an offset
    // that nobody has worked out yet.
    const nameAt = e.gex_alloc(160) >>> 0;
    const len = e.gex_object_at ? e.gex_object_at(BigInt(pc), nameAt, 160) >>> 0 : 0;
    const where = len
      ? new TextDecoder().decode(new Uint8Array(gex.memory.buffer, nameAt, len))
      : pc;
    console.error(`[hot] ${count.padStart(10)}  ${where}`);
  }
}
// GEX_FMA_COUNT reports how many fused multiply-adds crossed out of the guest
// module. wasm has no FMA, so each one is a call into the shim's.
if (process.env.GEX_FMA_COUNT) {
  const n = gex.shim?.exports?.gex_fma_calls?.();
  if (n !== undefined) process.stderr.write(`[gex] fma calls: ${n}\n`);
}
// Anything the program was asked to leave behind, written out for the caller.
for (const [guest, host] of extract) {
  const bytes = gex.readFile(guest);
  if (bytes) writeFileSync(host, bytes);
}
// `process.exitCode`, not `process.exit`. The guest's output goes through
// `process.stdout`/`process.stderr` -- it has to, so that this runner works
// inside a Worker, whose fd 1 is the *process's* -- and those are
// asynchronous when they are a pipe. `process.exit` does not wait for them,
// so whatever had not drained was thrown away: `apt update` appeared to stop
// after its first download, with the rest of its output and its error
// message both lost, and the run looked like a hang that had exited 0.
//
// Setting the code instead lets node finish the writes and leave on its own.
process.exitCode = status;
