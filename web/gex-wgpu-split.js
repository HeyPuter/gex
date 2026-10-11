// Mesa's cold part (docs/mesa-wasm-size.md): a --pre-js of gex-wgpu-replay.
//
// The build splits the module with Binaryen's wasm-split (web/mesa-split.py).
// The functions the page's workloads run stay in gex-wgpu-replay.wasm; the
// rest moves to gex-wgpu-replay.deferred.wasm, and each table slot that would
// reach one of them holds a trampoline instead. A trampoline calls the import
// `gexcold.load` below, which instantiates the whole cold part over those
// slots, and then calls through its slot again: the function itself now. So
// a miss costs one load per instance of the module (the page's or Worker's
// that made it, and a pthread's own should Mesa run code there), not one per
// function, and leaves no JS frame on the stack that JSPI would have to
// suspend across. (Emscripten's own placeholders are JS functions that make
// the call themselves; web/mesa-split.py replaces them.)
//
// Two ways in, both here because this file runs in every realm the module
// does:
//
// - Prefetch. Once the module has presented a frame, the realm fetches the
//   cold part and compiles it asynchronously, in the background. A miss
//   after that only instantiates it.
// - A miss before then loads it synchronously: a synchronous XHR as text in
//   the x-user-defined charset, compiled synchronously, which a Worker may
//   do at any size.
//
// A page's main thread may not (Chromium refuses a synchronous compile or
// instantiation over 8 MB there), so on one the cold part is instantiated
// with the module, asynchronously, and never missed.
//
// The build sets the marker below when it split the module
// (web/build-mesa-wasm.sh); an unsplit build fetches nothing and changes
// nothing.
var gexMesaColdSplit = 'GEX_MESA_COLD_NO';
var gexMesaCold = function () {
  return globalThis.__gexMesaCold || (globalThis.__gexMesaCold = {});
};
var gexMesaColdFile = function () {
  return wasmBinaryFile.slice(0, -5) + '.deferred.wasm';
};
var gexMesaColdInstance = null;

// `gexcold.load`: the cold part, instantiated over this instance's table.
var gexMesaColdLoad = function () {
  if (gexMesaColdInstance) return;
  var t0 = performance.now();
  var file = gexMesaColdFile();
  var cache = gexMesaCold();
  var module = cache[file];
  var how = 'prefetched';
  if (!(module instanceof WebAssembly.Module)) {
    var xhr = new XMLHttpRequest();
    xhr.open('GET', file, false);
    xhr.overrideMimeType('text/plain; charset=x-user-defined');
    xhr.send(null);
    if (xhr.status !== 200 && xhr.status !== 0) {
      throw new Error("Mesa's cold part " + file + ': HTTP ' + xhr.status);
    }
    var text = xhr.responseText;
    var buf = new Uint8Array(text.length);
    for (var i = 0; i < text.length; i++) buf[i] = text.charCodeAt(i) & 0xff;
    module = cache[file] = new WebAssembly.Module(buf);
    how = 'fetched and compiled now, ' + buf.length + ' bytes';
  }
  gexMesaColdInstance = new WebAssembly.Instance(module, { primary: wasmRawExports });
  console.warn('[gex-wgpu] cold part loaded on a miss (' + how + ') in ' +
               (performance.now() - t0).toFixed(1) + ' ms');
};

if (gexMesaColdSplit === 'GEX_MESA_COLD_YES' && !Module['instantiateWasm']) {
  // The glue's own instantiation, with the trampolines' import added.
  Module['instantiateWasm'] = function (imports, done) {
    var cold = { load: gexMesaColdLoad };
    var all = new Proxy(imports, {
      get: function (target, ns) { return ns === 'gexcold' ? cold : target[ns]; },
    });
    if (typeof ENVIRONMENT_IS_PTHREAD !== 'undefined' && ENVIRONMENT_IS_PTHREAD) {
      // A pthread instantiates the module the page sent it, and has no file
      // name of its own for the cold part's to follow.
      wasmBinaryFile ??= findWasmBinary();
      done(new WebAssembly.Instance(wasmModule, all), wasmModule);
      return {};
    }
    wasmBinaryFile ??= findWasmBinary();
    var file = wasmBinaryFile;
    var slow = function () {
      return fetch(file, { credentials: 'same-origin' })
        .then(function (r) { return r.arrayBuffer(); })
        .then(function (bytes) { return WebAssembly.instantiate(bytes, all); });
    };
    // A page's main thread may not compile or instantiate a module over
    // 8 MB synchronously, so there the cold part is not left for a miss: it
    // is instantiated now, asynchronously, before the module starts. (The
    // pages that run Mesa there are demos and test harnesses; the desktop's
    // compositor and SP's system Worker are Workers, and load it lazily.)
    // `gexColdLazy` keeps it lazy anyway, for measuring a miss.
    var eager = typeof ENVIRONMENT_IS_WEB !== 'undefined' && ENVIRONMENT_IS_WEB &&
      !Module['gexColdLazy'];
    WebAssembly.instantiateStreaming(fetch(file, { credentials: 'same-origin' }), all)
      .catch(slow)
      .then(function (r) {
        if (!eager) return r;
        var t0 = performance.now();
        var cold = gexMesaColdFile();
        var imp = { primary: r.instance.exports };
        return WebAssembly.instantiateStreaming(fetch(cold, { credentials: 'same-origin' }), imp)
          .catch(function () {
            return fetch(cold, { credentials: 'same-origin' })
              .then(function (x) { return x.arrayBuffer(); })
              .then(function (bytes) { return WebAssembly.instantiate(bytes, imp); });
          })
          .then(function (c) {
            gexMesaColdInstance = c.instance;
            console.warn('[gex-wgpu] cold part loaded with the module (a page\'s main thread) in ' +
                         (performance.now() - t0).toFixed(1) + ' ms');
            return r;
          });
      })
      .then(function (r) { done(r.instance, r.module); },
            function (e) { abort('instantiating Mesa failed: ' + e); });
    return {};
  };
}

if (gexMesaColdSplit === 'GEX_MESA_COLD_YES' && !Module['gexNoColdPrefetch']) {
  // Wait for the first presented frame, then fetch and compile in the
  // background. `_gex_gl_presented` reads a counter in shared memory, so a
  // pthread sees the page's frames too; one that never returns to its event
  // loop never gets here, and a miss there compiles the cold part
  // synchronously, which a Worker may do.
  var gexColdPoll = setInterval(function () {
    if (typeof _gex_gl_presented !== 'function' || typeof wasmBinaryFile !== 'string') return;
    var presented = 0;
    try { presented = _gex_gl_presented(); } catch (e) { return; }
    if (!presented) return;
    clearInterval(gexColdPoll);
    if (gexMesaColdInstance) return;
    var file = gexMesaColdFile();
    var cache = gexMesaCold();
    if (cache[file]) return;
    cache[file] = 'pending';
    var t0 = performance.now(), size = 0;
    fetch(file).then(function (r) {
      if (!r.ok) throw new Error('HTTP ' + r.status);
      return r.arrayBuffer();
    }).then(function (bytes) {
      size = bytes.byteLength;
      return WebAssembly.compile(bytes);
    }).then(function (module) {
      if (!(cache[file] instanceof WebAssembly.Module)) cache[file] = module;
      console.warn('[gex-wgpu] cold part prefetched (' + size + ' bytes) in ' +
                   (performance.now() - t0).toFixed(1) + ' ms');
    }, function (e) {
      if (cache[file] === 'pending') delete cache[file];
      console.warn('[gex-wgpu] cold part prefetch failed: ' + e);
    });
  }, 1000);
}
