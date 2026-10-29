// gex-embed's JS side: per-Worker state, which outlives the pthreads Rust's thread-locals belong to.
//
// Each Worker has its own copy of gex's dispatch tables (a WebAssembly.Table can't be shared between Workers) and its
// own instance of every translated module, installed from the module log in linear memory (`src/jit.rs`). The
// functions translated code imports are static functions of the host module: their function-table indices are the
// same on every Worker, so `wasmTable.get` gives each module the host's own function, and a call is wasm to wasm.
//
// The descriptor (`src/lib.rs`, `Desc`), as u32s: seed, seed_len, rows, log_len, row_size, then the imports.

var LibraryGexEmbed = {
  $GEX: {
    // Set by `gexInit` on a Worker's first call.
    imports: null,
    tables: null,
    stub: null,
    enter: null,
    // Modules of the log this Worker has instantiated.
    installed: 0,
  },

  $gexInit__deps: ['$GEX'],
  $gexInit: (desc) => {
    if (GEX.enter) return;
    const d = desc >> 2;
    const names = ['syscall', 'dispatch_miss', 'illegal', 'breakpoint', 'sysreg_read', 'sysreg_write', 'cache_op',
                   'safepoint', 'fma', 'x87'];
    const imports = { memory: wasmMemory };
    names.forEach((n, k) => { imports[n] = wasmTable.get(HEAPU32[d + 5 + k]); });
    const seed = HEAPU32[d], seedLen = HEAPU32[d + 1];
    const inst = new WebAssembly.Instance(new WebAssembly.Module(HEAPU8.slice(seed, seed + seedLen)), { gex: imports });
    const tables = [];
    for (let t = 0; t < 8; t++) {
      const name = t ? 'dispatch' + t : 'dispatch';
      tables.push(inst.exports[name]);
      imports[name] = inst.exports[name];
    }
    GEX.imports = imports;
    GEX.tables = tables;
    // Slot 0 of table 0 is the miss stub: what tables grow with, so a slot this Worker has not filled misses.
    GEX.stub = tables[0].get(0);
    GEX.enter = inst.exports.enter;
  },

  gexjs_sync__deps: ['$GEX', '$gexInit'],
  gexjs_sync__sig: 'ii',
  gexjs_sync: (desc) => {
    gexInit(desc);
    const d = desc >> 2;
    const rows = HEAPU32[d + 2], lenAt = HEAPU32[d + 3], rowSize = HEAPU32[d + 4];
    const n = Atomics.load(HEAPU32, lenAt >> 2);
    if (GEX.installed >= n) return 0;
    for (let i = GEX.installed; i < n; i++) {
      const r = (rows + i * rowSize) >> 2;
      const p = HEAPU32[r], len = HEAPU32[r + 1];
      for (let t = 0; t < 8; t++) {
        const need = HEAPU32[r + 2 + t], table = GEX.tables[t];
        if (table.length < need) table.grow(need - table.length, GEX.stub);
      }
      // Its active element segments write its blocks into this Worker's tables.
      new WebAssembly.Instance(new WebAssembly.Module(HEAPU8.slice(p, p + len)), { gex: GEX.imports });
    }
    GEX.installed = n;
    return 1;
  },

  gexjs_enter__deps: ['$GEX', '$gexInit'],
  gexjs_enter__sig: 'viij',
  gexjs_enter: (desc, ctx, pc) => {
    gexInit(desc);
    try {
      GEX.enter(ctx, pc);
    } catch (e) {
      // A trap in translated code. Say where the guest was before it unwinds through the host's frames.
      if (e instanceof WebAssembly.RuntimeError) {
        const q = (off) => '0x' + (BigInt(HEAPU32[(ctx + off) >> 2]) | (BigInt(HEAPU32[(ctx + off + 4) >> 2]) << 32n)).toString(16);
        err(`gex-embed: ${e.message} in guest code: pc ${q(256)} last target ${q(352)} x30 ${q(240)} sp ${q(248)}`);
        let regs = '';
        for (let i = 0; i < 30; i++) regs += ` x${i}=${q(i * 8)}`;
        err(`gex-embed:${regs}`);
      }
      throw e;
    }
  },
};

addToLibrary(LibraryGexEmbed);
