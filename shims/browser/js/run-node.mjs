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
  write(fd, bytes) { writeSync(fd, bytes); }
  now() { return BigInt(Date.now()) * 1000000n; }
}

const shimWasm = readFileSync(
  new URL('../../../target/wasm32-unknown-unknown/release/gex_browser.wasm', import.meta.url),
);
const gex = new Gex(new NodeEnv());
const args = [argv0 ?? elfPath, ...guestArgs];
const entry = await gex.load(shimWasm, readFileSync(guestPath), readFileSync(elfPath), files, args, {
  strace,
  // The suite compares this run against QEMU on the same machine, and QEMU
  // reports the real user.
  uid: process.getuid?.(),
  gid: process.getgid?.(),
  stdin: readStdin(),
  cwd,
});
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
if (process.env.GEX_TRACE_MISSES) {
  gex.shim.exports.gex_set_trace_misses?.(1);
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
process.exit(status);
