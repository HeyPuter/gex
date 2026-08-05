// Run the browser runtime under Node.
//
// The same `gex.mjs` a page would load; Node only supplies the file reads and
// a console. If this works, the browser path works, because nothing below is
// Node-specific.
//
//   node shims/browser/js/run-node.mjs <guest.wasm> <program.elf> [args...]

import { readFileSync, writeSync } from 'node:fs';
import { Gex, Env } from './gex.mjs';

const [, , guestPath, elfPath] = process.argv;
if (!guestPath || !elfPath) {
  console.error('usage: run-node.mjs <guest.wasm> <program.elf>');
  process.exit(2);
}

class NodeEnv extends Env {
  write(fd, bytes) { writeSync(fd, bytes); }
  now() { return BigInt(Date.now()) * 1000000n; }
}

const shimWasm = readFileSync(
  new URL('../../../target/wasm32-unknown-unknown/release/gex_browser.wasm', import.meta.url),
);
const gex = new Gex(new NodeEnv());
const entry = await gex.load(shimWasm, readFileSync(guestPath), readFileSync(elfPath));
process.exit(gex.run(entry));
