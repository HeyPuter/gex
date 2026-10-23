#!/usr/bin/env node
// The door's views of a client's memory (gex.mjs `memoryViews`) cover all
// of it, in a 64-bit memory past 4 GiB too: the word view's length was
// `byteLength >>> 2`, which wraps there (4 GiB + 64 KiB gave 16384 words).
//
//   node tests/browser-door-views.mjs
//
// Prints "ok browser-door-views" or "FAIL browser-door-views: ...". Needs
// node with memory64 (the page's own); skips without it.
import { memoryViews } from '../shims/browser/js/gex.mjs';

const fail = (why) => { console.log(`FAIL browser-door-views: ${why}`); process.exit(1); };
const check = (what, buf) => {
  const v = memoryViews(buf);
  if (v.u8.length !== buf.byteLength) fail(`${what}: byte view ${v.u8.length} of ${buf.byteLength}`);
  if (v.i32.length !== Math.floor(buf.byteLength / 4)) fail(`${what}: word view ${v.i32.length} of ${buf.byteLength / 4}`);
  // The last word reads what the byte view wrote there.
  const at = buf.byteLength - 4;
  v.dv.setUint32(at, 0x5a5aa5a5, true);
  if (v.i32[at / 4] >>> 0 !== 0x5a5aa5a5) fail(`${what}: the last word reads ${v.i32[at / 4] >>> 0}`);
};

check('32-bit memory, 64 KiB', new WebAssembly.Memory({ initial: 1, maximum: 1, shared: true }).buffer);
let big;
try {
  big = new WebAssembly.Memory({ address: 'i64', initial: 65537n, maximum: 65537n, shared: true }).buffer;
} catch (e) {
  console.log(`ok browser-door-views (skip: no 64-bit memory here: ${e.message})`);
  process.exit(0);
}
check('64-bit memory, 4 GiB + 64 KiB', big);
console.log('ok browser-door-views');
