// JSPI bridge for synchronous Gallium readback.  The exported smoke/replay
// entry must be wrapped with WebAssembly.promising by its JavaScript caller.
// A profiling build (wasm-split --instrument --in-memory; GEX_MESA_PROFILE=1)
// exports __write_profile: a flag per defined function, set the first time it
// runs. Every 2 s, when the set has grown, the module prints it as a bitmap --
// `[gex-wgpu] mesa-profile N HITS BASE64`, bit i for defined function i -- to
// the console, where the test harnesses already keep `gex-wgpu` warnings.
// Every other build exports no such function and this is never called.
var GEX_PROFILE_JS = `
function gexReportProfile(write) {
  var last = -1;
  var dump = function () {
    var need = write(0, 0);
    var at = _malloc(need);
    write(at, need);
    var n = (need - 8) >> 2, bits = new Uint8Array((n + 7) >> 3), hits = 0;
    if (typeof growMemViews == "function") growMemViews();
    var words = new Uint32Array(HEAPU8.slice(at + 8, at + 8 + n * 4).buffer);
    _free(at);
    for (var i = 0; i < n; i++) if (words[i]) { bits[i >> 3] |= 1 << (i & 7); hits++; }
    if (hits === last) return;
    last = hits;
    // In pieces of 2 KiB (2,732 characters of base64): page harnesses cut
    // console lines at about 4,000, and a 25,000-function set is 4,120.
    // Each piece says where it starts, in bytes: \`@OFFSET\`.
    for (var at0 = 0; at0 < bits.length; at0 += 2048) {
      var bin = '';
      for (var j = at0; j < Math.min(bits.length, at0 + 2048); j++) bin += String.fromCharCode(bits[j]);
      console.warn('[gex-wgpu] mesa-profile ' + n + ' ' + hits + ' @' + at0 + ' ' + btoa(bin));
    }
  };
  setInterval(dump, 2000);
  Module['gexDumpProfile'] = dump;
}
`;
mergeInto(LibraryManager.library, {
  gex_wgpu_jspi_wait: function () {
    // A list, not one slot. WebGPU dispatches its map callbacks back into the
    // suspended module, so a callback that leads to another readback starts a
    // second suspend inside the first -- and a single slot meant the inner
    // one overwrote the outer's resolver, leaving the outer suspended for
    // ever. Every waiter is woken and re-checks its own `done` flag, which is
    // what the callers already do.
    return new Promise(function (resolve) {
      var waiting = Module['__gexWgpuWaiting'] || (Module['__gexWgpuWaiting'] = []);
      waiting.push(resolve);
    });
  },
  // Two targets share this bridge: the Gallium corpus, whose entry is
  // gex_wgpu_smoke, and the wire-decoder replay, whose entry is gex_gl_submit.
  // Publish whichever the linked module actually exports -- the raw wasm
  // export, because WebAssembly.promising cannot wrap the JS-side wrapper.
  gex_wgpu_jspi_wait__postset:
    '_gex_wgpu_jspi_wait = new WebAssembly.Suspending(_gex_wgpu_jspi_wait); addOnPostRun(function() { if (wasmExports[\'gex_wgpu_smoke\']) Module[\'gexWgpuRaw\'] = wasmExports[\'gex_wgpu_smoke\']; if (wasmExports[\'gex_gl_submit\']) Module[\'gexGlSubmitRaw\'] = wasmExports[\'gex_gl_submit\']; if (wasmExports[\'__write_profile\']) gexReportProfile(wasmExports[\'__write_profile\']); });' +
    GEX_PROFILE_JS,

  gex_wgpu_jspi_signal: function () {
    var waiting = Module['__gexWgpuWaiting'];
    if (!waiting || !waiting.length) return;
    Module['__gexWgpuWaiting'] = [];
    for (var i = 0; i < waiting.length; ++i) waiting[i]();
  },
});
