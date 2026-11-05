// Mesa's cold part (ports/mesa/split.py): a --pre-js of gex-wgpu-replay.
//
// The build splits the module with Binaryen's wasm-split (ports/mesa/split.py).
// The functions the page's workloads run stay in gex-wgpu-replay.wasm; the
// rest moves to gex-wgpu-replay.deferred.wasm, and each table slot that would
// reach one of them holds a trampoline instead. A trampoline calls the import
// `gexcold.load` below, which instantiates the whole cold part over those
// slots, and then calls through its slot again: the function itself now. So
// a miss costs one load per instance of the module (the page's or Worker's
// that made it, and a pthread's own should Mesa run code there), not one per
// function, and leaves no JS frame on the stack that JSPI would have to
// suspend across. (Emscripten's own placeholders are JS functions that make
// the call themselves; ports/mesa/split.py replaces them.)
//
// Where it is loaded:
//
// - With the module. The glue's instantiation fetches and compiles the cold
//   part beside the module, both streaming, and instantiates it before the
//   module starts, in the page's main thread and a Worker alike: a page's
//   main thread may not compile a module over 8 MB synchronously (Chromium
//   refuses), and a Worker's first frames miss into it.
// - On a miss, where it was not (`gexColdLazy`, a pthread, a caller's own
//   instantiation): a synchronous XHR as text in the x-user-defined charset,
//   compiled synchronously, which a Worker may do at any size. Once the
//   module has presented a frame, a realm without it prefetches it in the
//   background, so a later miss only instantiates it.
//
// The build sets the marker below when it split the module
// (ports/mesa/build.sh); an unsplit build fetches nothing and changes
// nothing.
var gexMesaColdSplit = 'GEX_MESA_COLD_NO';
var gexMesaCold = function () {
  return globalThis.__gexMesaCold || (globalThis.__gexMesaCold = {});
};
// By its own name, as the glue finds its module (`locateFile`): a page or
// package that names its files where it puts them (a bundler's content
// hashes) answers for this one too, where the module's URL with its suffix
// swapped would name a file nobody wrote.
var gexMesaColdFile = function () {
  return locateFile('gex-wgpu-replay.deferred.wasm');
};
var gexMesaColdInstance = null;
// How the cold part came in, for a caller to read (`Module.gexColdLoaded`):
// 'with the module' or 'on a miss'; unset while it has not. A property, not
// a log line: loading it with the module is every start's.
var gexMesaColdLoaded = function (how) {
  Module['gexColdLoaded'] = how;
};

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
  gexMesaColdLoaded('on a miss');
  // Who missed: the frames under the trampoline, for ports/mesa/hot.txt.
  var where = String(new Error().stack).split('\n').filter(function (l) {
    return l.indexOf('.wasm') >= 0 && l.indexOf('gexcoldtramp') < 0;
  }).slice(0, 4).map(function (l) { return l.trim().replace(/^at /, '').replace(/ \(.*$/, ''); });
  console.warn('[gex-wgpu] cold part loaded on a miss (' + how + ') in ' +
               (performance.now() - t0).toFixed(1) + ' ms, from ' + (where.join(' <- ') || '?'));
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
    // The cold part is not left for a miss: it is fetched and compiled
    // beside the module, streaming, and instantiated before the module
    // starts. A page's main thread may not compile a module over 8 MB
    // synchronously, so a miss there could not load it at all; and in a
    // Worker (the compositor's, SP's system Worker) the first frames miss
    // into it, so a miss's synchronous fetch and compile, 120 ms of a
    // 10 MB part on gl/gears, came in the middle of the start.
    // `gexColdLazy` keeps it lazy anyway, for measuring a miss.
    var eager = !Module['gexColdLazy'];
    // The cold part's fetch and compile start with the module's, both
    // streaming, not after it: it is instantiated once the module is.
    var coldFile = eager ? gexMesaColdFile() : null;
    var coldModule = eager ? WebAssembly.compileStreaming(fetch(coldFile, { credentials: 'same-origin' }))
      .catch(function () {
        return fetch(coldFile, { credentials: 'same-origin' })
          .then(function (x) { return x.arrayBuffer(); })
          .then(function (bytes) { return WebAssembly.compile(bytes); });
      }) : null;
    // Awaited below; a module that fails first leaves it unawaited.
    if (coldModule) coldModule.catch(function () {});
    WebAssembly.instantiateStreaming(fetch(file, { credentials: 'same-origin' }), all)
      .catch(slow)
      .then(function (r) {
        if (!eager) return r;
        return coldModule
          .then(function (m) { return WebAssembly.instantiate(m, { primary: r.instance.exports }); })
          .then(function (instance) {
            gexMesaColdInstance = instance;
            gexMesaColdLoaded('with the module');
            return r;
          });
      })
      .then(function (r) { done(r.instance, r.module); },
            function (e) { abort('instantiating Mesa failed: ' + e); });
    return {};
  };
}

if (gexMesaColdSplit === 'GEX_MESA_COLD_YES' && !Module['gexNoColdPrefetch'] &&
    !(typeof ENVIRONMENT_IS_PTHREAD !== 'undefined' && ENVIRONMENT_IS_PTHREAD)) {
  // Fetch and compile in the background once the first frame is shown:
  // gexgl-mesa.cpp calls this from that frame, and it queues the work behind
  // it. Not in a pthread: the module's pool started each with this file, Mesa
  // runs no code there, and every one of them fetched and compiled its own
  // copy (four, on an mt build). A miss on a pthread would compile the cold
  // part synchronously, which a Worker may do.
  Module['gexColdFirstFrame'] = function () {
    setTimeout(function () {
      if (gexMesaColdInstance || typeof wasmBinaryFile !== 'string') return;
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
    }, 0);
  };
}
