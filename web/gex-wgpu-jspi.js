// JSPI bridge for synchronous Gallium readback.  The exported smoke/replay
// entry must be wrapped with WebAssembly.promising by its JavaScript caller.
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
    '_gex_wgpu_jspi_wait = new WebAssembly.Suspending(_gex_wgpu_jspi_wait); addOnPostRun(function() { if (wasmExports[\'gex_wgpu_smoke\']) Module[\'gexWgpuRaw\'] = wasmExports[\'gex_wgpu_smoke\']; if (wasmExports[\'gex_gl_submit\']) Module[\'gexGlSubmitRaw\'] = wasmExports[\'gex_gl_submit\']; });',

  gex_wgpu_jspi_signal: function () {
    var waiting = Module['__gexWgpuWaiting'];
    if (!waiting || !waiting.length) return;
    Module['__gexWgpuWaiting'] = [];
    for (var i = 0; i < waiting.length; ++i) waiting[i]();
  },
});
