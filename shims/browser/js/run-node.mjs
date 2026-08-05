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
//     --argv0 <name>         what the guest sees as argv[0]
//     --strace               narrate every syscall to stderr

import { readdirSync, readFileSync, statSync, writeFileSync, writeSync } from 'node:fs';
import { Gex, Env } from './gex.mjs';

// Options come first and are named, so a guest argument that happens to
// contain an `=` -- `lua -e 'local x=1'`, say -- reaches the guest intact.
const files = {};
let strace = false;
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
        for (const name of readdirSync(host)) {
          const h = `${host}/${name}`, g = `${guest}/${name}`;
          if (statSync(h).isDirectory()) walk(g, h);
          else files[g] = readFileSync(h);
        }
      };
      walk(guestRoot.replace(/\/$/, ''), hostRoot.replace(/\/$/, ''));
    } else if (av[i] === '--extract') {
      const spec = av[++i] ?? '';
      const eq = spec.indexOf('=');
      if (eq <= 0) { console.error('--extract wants <guest>=<host>'); process.exit(2); }
      extract.push([spec.slice(0, eq), spec.slice(eq + 1)]);
    } else if (av[i] === '--strace') strace = true;
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
});
const status = await gex.run(entry);
// Anything the program was asked to leave behind, written out for the caller.
for (const [guest, host] of extract) {
  const bytes = gex.readFile(guest);
  if (bytes) writeFileSync(host, bytes);
}
process.exit(status);
