// Reading the signatures out of a .wasm, without instantiating it.
//
// A diagnostic, not part of the runtime. It exists because the 32- and 64-bit
// shims have to present JS with exactly the same ABI -- if any export or
// import changed width between them, the glue would need a conversion layer
// on every call, and the sensible fix is always to change the Rust signature
// instead. This is what checks that, and it found the two that had drifted:
// `gex_alloc` and `gex_free` took a `usize` length, which is an i64 on wasm64
// and so would have demanded a BigInt from JS for those two functions alone.
// They take a `u32` now and the two ABIs are identical, 97 exports and 75
// imports each.
//
// V8 ships no type reflection -- a WebAssembly.Function will not say what it
// takes -- so the signatures are read out of the module bytes directly.
//
// To compare two builds:
//
//   node -e "import('./shims/browser/js/wasm-sig.mjs').then(...)"
//
// or just call `exportSignatures`/`importSignatures` and diff the maps.

const SEC_TYPE = 1;
const SEC_IMPORT = 2;
const SEC_FUNC = 3;
const SEC_EXPORT = 7;

export const I64 = 0x7e;

// A reader that only knows the three things a section header needs: unsigned
// LEB128, a byte, and a length-prefixed name.
class Cursor {
  constructor(bytes) {
    this.b = bytes;
    this.i = 0;
  }
  u8() {
    return this.b[this.i++];
  }
  leb() {
    let r = 0;
    let s = 0;
    for (;;) {
      const c = this.b[this.i++];
      r |= (c & 0x7f) * 2 ** s;
      if (!(c & 0x80)) return r;
      s += 7;
    }
  }
  name() {
    const n = this.leb();
    const s = new TextDecoder().decode(this.b.subarray(this.i, this.i + n));
    this.i += n;
    return s;
  }
  skip(n) {
    this.i += n;
  }
}

// Map from exported function name to {params, results}, each an array of
// valtype bytes. Non-function exports are absent.
export function exportSignatures(bytes) {
  const c = new Cursor(bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes));
  c.skip(8); // magic and version

  const types = []; // index -> {params, results}
  const funcTypes = []; // function index -> type index, imports first
  const out = new Map();
  let exportSection = null;

  while (c.i < c.b.length) {
    const id = c.u8();
    const len = c.leb();
    const end = c.i + len;

    if (id === SEC_TYPE) {
      const n = c.leb();
      for (let t = 0; t < n; t++) {
        const form = c.u8();
        // 0x60 is a plain function type; the GC proposal wraps it in 0x4e/0x50
        // recursion and subtype forms, which the shim never emits.
        if (form !== 0x60) {
          c.i = end;
          break;
        }
        const np = c.leb();
        const params = [];
        for (let p = 0; p < np; p++) params.push(c.u8());
        const nr = c.leb();
        const results = [];
        for (let r = 0; r < nr; r++) results.push(c.u8());
        types.push({ params, results });
      }
    } else if (id === SEC_IMPORT) {
      const n = c.leb();
      for (let k = 0; k < n; k++) {
        c.name();
        c.name();
        const kind = c.u8();
        if (kind === 0x00) funcTypes.push(c.leb());
        else if (kind === 0x01) {
          c.u8(); // table elemtype
          const flags = c.u8();
          c.leb();
          if (flags & 1) c.leb();
        } else if (kind === 0x02) {
          const flags = c.u8();
          c.leb();
          if (flags & 1) c.leb();
        } else if (kind === 0x03) {
          c.u8(); // global valtype
          c.u8(); // mutability
        }
      }
    } else if (id === SEC_FUNC) {
      const n = c.leb();
      for (let k = 0; k < n; k++) funcTypes.push(c.leb());
    } else if (id === SEC_EXPORT) {
      // The export section can precede the function section in a valid
      // module, so remember where it was and walk it once everything is read.
      exportSection = c.b.subarray(c.i, end);
    }

    c.i = end;
  }

  if (exportSection) {
    const e = new Cursor(exportSection);
    const n = e.leb();
    for (let k = 0; k < n; k++) {
      const nm = e.name();
      const kind = e.u8();
      const idx = e.leb();
      if (kind === 0x00 && types[funcTypes[idx]]) out.set(nm, types[funcTypes[idx]]);
    }
  }

  return out;
}

// Map from "module.name" to {params, results} for every imported function.
export function importSignatures(bytes) {
  const c = new Cursor(bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes));
  c.skip(8);

  const types = [];
  const out = new Map();

  while (c.i < c.b.length) {
    const id = c.u8();
    const len = c.leb();
    const end = c.i + len;

    if (id === SEC_TYPE) {
      const n = c.leb();
      for (let t = 0; t < n; t++) {
        if (c.u8() !== 0x60) break;
        const np = c.leb();
        const params = [];
        for (let p = 0; p < np; p++) params.push(c.u8());
        const nr = c.leb();
        const results = [];
        for (let r = 0; r < nr; r++) results.push(c.u8());
        types.push({ params, results });
      }
    } else if (id === SEC_IMPORT) {
      const n = c.leb();
      for (let k = 0; k < n; k++) {
        const mod = c.name();
        const nm = c.name();
        const kind = c.u8();
        if (kind === 0x00) {
          const t = c.leb();
          if (types[t]) out.set(`${mod}.${nm}`, types[t]);
        } else if (kind === 0x01) {
          c.u8();
          const flags = c.u8();
          c.leb();
          if (flags & 1) c.leb();
        } else if (kind === 0x02) {
          const flags = c.u8();
          c.leb();
          if (flags & 1) c.leb();
        } else if (kind === 0x03) {
          c.u8();
          c.u8();
        }
      }
      // The type section always precedes the import section, so everything
      // needed is already read; nothing after this matters.
      break;
    }

    c.i = end;
  }

  return out;
}
