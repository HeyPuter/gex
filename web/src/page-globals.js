// The runtime's globals, set from the page's switches: app.js `start()`
// calls it with the build's switches and its options'. Not part of the app
// library's API (web/src/README.md).
import { pageOptions } from '../../shims/browser/js/options.mjs';

/**
 * The runtime's globals from its switches (`get(name)`): what gex.mjs,
 * runtime.js and the Workers read. One list, for the library and the test
 * page; its own module so a test can drive it without a page.
 */
export function setGlobals(get, b = {}) {
  // The switches every page takes (shims/browser/js/options.mjs).
  pageOptions(get);
  // The guest's architecture: x86 unless the build says aarch64 (a Linux
  // tree of Debian's arm64 packages), which picks the aarch64 shim, seed and
  // kernel (runtime.js).
  globalThis.GEX_ARCH = b.arch === 'aarch64' ? 'aarch64' : 'x86';
  if (get('heartbeat')) globalThis.GEX_HEARTBEAT = Number(get('heartbeat'));
  if (get('mem64') === '1' || b.mem64) globalThis.GEX_MEM64 = 1;
  // The function JIT: off by default (it doubles a 32-bit Wine program's
  // start-up).
  const fjit = get('fjit');
  if (fjit !== null && fjit !== '0') globalThis.GEX_FJIT = Number(fjit) || 1;
  if (get('elfunits') && get('cgen') !== '0') globalThis.GEX_ELF_UNITS = get('elfunits');
  if (get('pins')) globalThis.GEX_PINS = get('pins');
  if (get('dense') === '0') globalThis.GEX_DENSE = 0;
  if (get('dense') === '2') globalThis.GEX_DENSE = 2;
  // cgen=1|2 (prefer|require units), the unguarded flavour unless unguarded=0.
  // icallcount=1: the counting flavour of units (16), whose run prints the
  // indirect-call profile (cgen.rs gex_icall_dump; GEX_CGEN_ICALL_COUNT=1 units).
  // cgen=0 is off: no unit of any kind, the ELF objects' among them.
  const cgen = get('cgen');
  if (cgen && cgen !== '0') globalThis.GEX_CGEN = Number(cgen) | (get('unguarded') === '0' ? 0 : 4) | (get('icallcount') === '1' ? 16 : 0);
  if (get('cgenurl')) globalThis.GEX_CGEN_URL = get('cgenurl');
  // cgenzst=1: each unit's module is served as KEY/c.wasm.zst, decoded before it is compiled.
  if (get('cgenzst') === '1') globalThis.GEX_CGEN_ZST = 1;
  if (get('aot') === '1') globalThis.GEX_NO_JIT = 1;
  if (get('aot')) globalThis.GEX_AOT = 1;
  globalThis.GEX_UNIT_CACHE = get('unitcache') !== null ? Number(get('unitcache')) : 1;
}
