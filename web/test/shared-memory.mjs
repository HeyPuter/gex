// The same guest, on a plain memory and on a SharedArrayBuffer.
//
//   node web/test/shared-memory.mjs <static-aarch64-elf>
//
// The `shared` flag is part of a memory import's type, so a module built for
// one will not instantiate against the other; it cannot be a runtime choice,
// and the page picks by whether it is cross-origin isolated. The shared build
// is for threads -- a process's threads are Workers on its one shared memory.
//
// Running the same program both ways is the point: the shared build has a
// different allocator (a spinlock, since more than one thread can be inside the
// module) and its translator must emit modules that import a shared memory too
// -- a mismatch is a LinkError at the first dispatch miss, far from the cause.
import { readFile, access } from 'node:fs/promises';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, '..', '..');
const REL = join(ROOT, 'target/wasm32-unknown-unknown/release');
const BUILDS = {
  plain: { shim: join(REL, 'gex_browser.wasm'), seed: join(ROOT, 'web/public/gex/seed.wasm') },
  shared: {
    shim: join(REL, 'gex_browser.shared.wasm'),
    seed: join(ROOT, 'web/public/gex/seed.shared.wasm'),
  },
};

const exists = async (p) => access(p).then(() => true, () => false);
const skip = (why) => {
  console.log(`skip: ${why}`);
  process.exit(0);
};

const elfPath = process.argv[2];
if (!elfPath || !(await exists(elfPath))) skip('give it a static aarch64 program');
if (!(await exists(BUILDS.plain.shim))) skip('no browser shim (shims/browser/build.sh)');
if (!(await exists(BUILDS.shared.shim))) {
  skip('no shared-memory shim -- needs a nightly toolchain with rust-src');
}
if (!(await exists(BUILDS.shared.seed))) skip('no shared seed (web/build-assets.sh)');

const { Gex, Env } = await import(join(ROOT, 'shims/browser/js/gex.mjs'));
const elf = new Uint8Array(await readFile(elfPath));

async function run(kind) {
  let output = '';
  class Collect extends Env {
    write(fd, bytes) {
      // `.slice()` first: a view onto a SharedArrayBuffer is not something
      // every TextDecoder will take.
      output += new TextDecoder().decode(bytes.slice());
    }
    now() {
      return BigInt(Date.now()) * 1000000n;
    }
  }
  const { shim, seed } = BUILDS[kind];
  const gex = new Gex(new Collect(), { shared: kind === 'shared' });
  const entry = await gex.load(
    new Uint8Array(await readFile(shim)),
    new Uint8Array(await readFile(seed)),
    elf,
    {},
    [elfPath],
    {},
  );
  const status = await gex.run(entry);
  return { status, output, buffer: gex.memory.buffer.constructor.name };
}

const plain = await run('plain');
const shared = await run('shared');

if (plain.buffer !== 'ArrayBuffer' || shared.buffer !== 'SharedArrayBuffer') {
  console.log(`FAIL shared-memory: got ${plain.buffer} and ${shared.buffer}`);
  process.exit(1);
}
if (plain.status !== 0 || shared.status !== 0) {
  console.log(`FAIL shared-memory: exit ${plain.status} plain, ${shared.status} shared`);
  process.exit(1);
}
if (plain.output !== shared.output) {
  console.log('FAIL shared-memory: the two builds disagree');
  console.log(`     plain:  ${JSON.stringify(plain.output)}`);
  console.log(`     shared: ${JSON.stringify(shared.output)}`);
  process.exit(1);
}
console.log('ok   shared-memory (same output on an ArrayBuffer and a SharedArrayBuffer)');
process.exit(0);
