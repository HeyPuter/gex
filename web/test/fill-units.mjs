// Late units in the background fill (eject-site.mjs --late, lazy-fetch.mjs
// `fillFile`): a unit another Worker can publish must be loadable by every
// Worker of the process, including one started before the page compiled it.
// Such a Worker reads the unit's bytes from the fill, waiting for them if
// they have not arrived; before the fill carried them (units-late.tar, given
// only to Workers started after), it trapped ("published but cannot be
// loaded here", PF's E7b).
//
//   node web/test/fill-units.mjs
//
// Checked here, a Worker thread against a fill the main thread writes slowly:
// the Worker's read of a late unit's c.wasm blocks until those bytes are in,
// returns an unshared copy that compiles; an entry the fill does not have is
// null at once (no wait); a failed fill ends a wait with null.
import { Worker } from 'node:worker_threads';

const FETCH = new URL('../../shims/browser/js/lazy-fetch.mjs', import.meta.url).href;
// The smallest module: (module (func (export "f") (result i32) i32.const 7))
const WASM = Uint8Array.from([0, 97, 115, 109, 1, 0, 0, 0, 1, 5, 1, 96, 0, 1, 127, 3, 2, 1, 0, 7, 5, 1, 1, 102, 0, 0, 10, 6, 1, 4, 0, 65, 7, 11]);
const ENTRIES = new TextEncoder().encode('entries of K\n');
const LAZY = new Uint8Array(4096).fill(0x5a);

let failed = 0;
const check = (ok, what) => { console.log(`${ok ? 'PASS' : 'FAIL'} ${what}`); if (!ok) failed++; };

function makeFill() {
  const parts = [['/project/build/web/lazy/aa', LAZY], ['unit:K/c.entries', ENTRIES], ['unit:K/c.wasm', WASM]];
  const files = [];
  let off = 0;
  for (const [name, b] of parts) { files.push([name, off, b.length]); off += b.length; }
  const buf = new SharedArrayBuffer(off);
  const at = new Int32Array(new SharedArrayBuffer(4));
  const bytes = new Uint8Array(buf);
  for (const [i, [, b]] of parts.entries()) bytes.set(b, files[i][1]);
  return { fill: { buf, at, files }, size: off };
}

function worker(fill, names) {
  const src = `
    const { parentPort, workerData } = require('node:worker_threads');
    import(workerData.fetch).then(({ setFill, fillFile }) => {
      setFill(workerData.fill);
      const out = [];
      for (const n of workerData.names) {
        const t0 = Date.now();
        const b = fillFile(n);
        let compiles = false;
        try { if (b && n.endsWith('.wasm')) compiles = new WebAssembly.Instance(new WebAssembly.Module(b)).exports.f() === 7; } catch {}
        out.push({ n, done: Date.now(), len: b ? b.length : -1, shared: !!b && b.buffer instanceof SharedArrayBuffer, compiles, ms: Date.now() - t0 });
      }
      parentPort.postMessage(out);
    });`;
  return new Promise((res, rej) => {
    const w = new Worker(src, { eval: true, workerData: { fetch: FETCH, fill, names } });
    w.once('message', (m) => { res(m); w.terminate(); });
    w.once('error', rej);
  });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// 1. The Worker starts before any of the fill is in, as winlogon's did.
{
  const { fill, size } = makeFill();
  const asked = worker(fill, ['unit:missing/c.wasm', 'unit:K/c.wasm', 'unit:K/c.entries']);
  await sleep(150);
  Atomics.store(fill.at, 0, LAZY.length); Atomics.notify(fill.at, 0);   // the lazy file only
  await sleep(150);
  const published = Date.now();
  Atomics.store(fill.at, 0, size); Atomics.notify(fill.at, 0);          // then the unit
  const [missing, wasm, entries] = await asked;
  check(missing.len === -1 && missing.ms < 100, `an entry the fill does not have is null at once (${missing.ms} ms)`);
  check(wasm.len === WASM.length && wasm.done >= published, `a late unit's c.wasm waits for its bytes (returned ${wasm.done - published} ms after they were written; waited ${wasm.ms} ms) and is whole (${wasm.len} bytes)`);
  check(!wasm.shared && wasm.compiles, `... an unshared copy that compiles and runs (shared ${wasm.shared}, runs ${wasm.compiles})`);
  check(entries.len === ENTRIES.length, `its c.entries, there already, at once (${entries.ms} ms)`);
}

// 2. A failed fill ends the wait: the Worker gets null, not a hang.
{
  const { fill } = makeFill();
  const asked = worker(fill, ['unit:K/c.wasm']);
  await sleep(150);
  Atomics.store(fill.at, 0, -1); Atomics.notify(fill.at, 0);
  const [wasm] = await asked;
  check(wasm.len === -1, `a failed fill: null after ${wasm.ms} ms`);
}

console.log(failed ? `FAIL ${failed} check(s)` : 'PASS all');
process.exit(failed ? 1 : 0);
