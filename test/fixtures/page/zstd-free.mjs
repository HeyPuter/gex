// The page's zstd decoder is let go when nothing decodes (page/src/zstd-loader.js,
// the vendored zstddec's `dispose`): its wasm memory, grown by the boot's
// large-window pieces, is collectable once the last user gives it back and
// the idle time passes, and the next user gets a working new one. Users that
// overlap share one; one taken during the idle time keeps it.
//
//   node --expose-gc test/fixtures/page/zstd-free.mjs
import { pathToFileURL } from 'node:url';
import { join } from 'node:path';
import { zstdCompressSync, constants } from 'node:zlib';
import { randomFillSync } from 'node:crypto';
import assert from 'node:assert/strict';

// gex: the checkout these fixtures are in, or GEX_ROOT.
const GEX_ROOT = process.env.GEX_ROOT || join(import.meta.dirname, '..', '..', '..');
if (typeof gc !== 'function') throw new Error('run with node --expose-gc');
if (typeof zstdCompressSync !== 'function') { console.log('skip zstd-free: this node has no zstd'); process.exit(0); }
const { zstdLoader } = await import(pathToFileURL(join(GEX_ROOT, 'page/src/zstd-loader.js')).href);
const src = pathToFileURL(join(GEX_ROOT, 'page/vendor/zstddec/zstddec-stream.mjs')).href;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// The loader's idle timer, by hand: its setTimeout of IDLE ms is queued here
// and run by `idle()`, so no step depends on how fast this machine runs a
// sleep (the loader's other timers, and this file's, are the real ones).
const IDLE = 200;
const realSet = globalThis.setTimeout, realClear = globalThis.clearTimeout;
const pending = new Map();
let nextTimer = 0;
globalThis.setTimeout = (fn, ms, ...a) => {
  if (ms !== IDLE) return realSet(fn, ms, ...a);
  const id = { idle: ++nextTimer };
  pending.set(id, () => fn(...a));
  return id;
};
globalThis.clearTimeout = (id) => (pending.delete(id) ? undefined : realClear(id));
/** The idle time passes: every idle timer queued now runs. */
const idle = () => { const due = [...pending.values()]; pending.clear(); for (const f of due) f(); };

// A 24 MiB input with a 2^25 window, half incompressible: the decoder's
// memory grows past 24 MiB, as for the windemo's large pieces.
const raw = Buffer.alloc(24 << 20);
randomFillSync(raw, 0, 12 << 20);
raw.copy(raw, 12 << 20, 0, 12 << 20);
const packed = zstdCompressSync(raw, { params: { [constants.ZSTD_c_windowLog]: 25, [constants.ZSTD_c_compressionLevel]: 3 } });

let memRef = null;
/** Decode `packed` through `d` in 1 MiB pushes; keeps only a weak hold on the decoder's memory. */
function decode(d) {
  const out = Buffer.alloc(raw.length);
  let n = 0;
  const z = d.stream();
  for (let at = 0; at < packed.length; at += 1 << 20) {
    z.push(packed.subarray(at, at + (1 << 20)), (part) => {
      memRef ??= new WeakRef(part.buffer);
      out.set(part, n);
      n += part.length;
    });
  }
  z.end();
  assert.equal(n, raw.length, 'decoded length');
  assert.ok(out.equals(raw), 'decoded bytes');
}
async function collected() {
  // Each gc in a job of its own: a WeakRef's target is kept to the end of the
  // job that made it or last read it.
  for (let i = 0; i < 20; i++) {
    await sleep(20);
    gc();
    if (!memRef.deref()) return true;
  }
  return false;
}

let failed = false;
const step = async (what, fn) => {
  try {
    await fn();
    console.log(`ok   zstd-free: ${what}`);
  } catch (e) {
    console.log(`FAIL zstd-free: ${what}: ${e.message}`);
    failed = true;
  }
};

const get = zstdLoader(src, { idleMs: IDLE });

await step('a decode, then the memory goes after the idle time', async () => {
  const d = await get();
  decode(d);
  assert.ok(memRef.deref().byteLength > (24 << 20), `the memory grew (${memRef.deref().byteLength} bytes)`);
  get.release();
  assert.equal(get.held(), true, 'kept during the idle time');
  idle();
  assert.equal(get.held(), false, 'let go after it');
  assert.equal(await collected(), true, 'the decoder\'s memory was not collected');
});

await step('the next user gets a new decoder that works', async () => {
  memRef = null;
  const d = await get();
  decode(d);
  get.release();
});

await step('users that overlap share one, kept until the last gives it back', async () => {
  idle();
  const [a, b] = await Promise.all([get(), get()]);
  assert.equal(a, b, 'one decoder');
  get.release();
  idle();
  assert.equal(get.held(), true, 'freed with a user left');
  decode(b);
  get.release();
  idle();
  assert.equal(get.held(), false);
});

await step('one taken during the idle time keeps it', async () => {
  const a = await get();
  get.release();
  const b = await get();
  assert.equal(a, b, 'the same decoder');
  idle();
  assert.equal(get.held(), true, 'freed while in use');
  decode(b);
  get.release();
  idle();
  assert.equal(get.held(), false);
});

await step('the loader\'s copy is its own: another importer of the URL keeps its instance', async () => {
  const { ZSTDDecoder } = await import(src);
  const other = new ZSTDDecoder();
  await other.init();
  const d = await get();
  get.release();
  idle();
  assert.equal(get.held(), false);
  memRef = null;
  decode(other);
  void d;
});

process.exit(failed ? 1 : 0);
