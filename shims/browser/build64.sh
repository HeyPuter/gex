#!/bin/sh
# Build the browser shim for a 64-bit guest memory.
#
# Separate from `build.sh` rather than a flag on it because almost nothing is
# shared: a different target, a nightly-only one, `-Z build-std` for the whole
# standard library, and an output that cannot be substituted for the 32-bit
# one anywhere -- a module importing an i64 memory will not link against an
# i32 one, so the host picks a shim and a guest module together or neither.
#
# The linker flags are the same as build.sh's and mean the same things; see
# there for why each is needed. The two extra facts about this target:
#
#   * `wasm64-unknown-unknown` is in `rustc --print target-list` but not in
#     `rustup target list`, so there is no `rustup target add` for it and the
#     standard library has to be built from source with `-Z build-std`.
#   * `--export-dynamic` is required. On wasm32 a cdylib exports its
#     `#[no_mangle] pub extern "C"` functions on its own; on wasm64 it does
#     not, and the link succeeds with a .wasm that exports nothing at all --
#     which surfaces much later as "the gex runtime is out of date: it has no
#     gex_alloc".
#
# `feature(simd_wasm64)` is what makes `core::arch::wasm64` available, which
# is where `memory_grow`, `memory_size` and the atomic wait/notify live for
# this target.
set -e
cd "$(dirname "$0")/../.."

GUEST_BYTES=$((0xFFFF0000))
GUEST_INITIAL=$((0x05000000))
SHIM_BASE=$((0x04800000))

LINK="-C link-arg=--import-memory \
  -C link-arg=--initial-memory=$GUEST_INITIAL \
  -C link-arg=--max-memory=$GUEST_BYTES \
  -C link-arg=--global-base=$SHIM_BASE \
  -C link-arg=-zstack-size=8388608 \
  -C link-arg=--export=__stack_pointer \
  -C link-arg=--export-dynamic"

if ! rustup toolchain list 2>/dev/null | grep -q nightly; then
    echo "shims/browser: wasm64 needs nightly, and there is none installed"
    exit 1
fi

OUT=target/wasm64-unknown-unknown/release/gex_browser.wasm
RUSTFLAGS="-Zcrate-attr=feature(simd_wasm64) $LINK" \
    cargo +nightly build -p gex-browser --target wasm64-unknown-unknown \
      --release -Z build-std=std,panic_abort
echo "shims/browser: $(stat -c%s "$OUT") bytes (64-bit memory)"

# The shim's ABI towards JavaScript has to be identical on both targets, or
# the glue would need a conversion layer on every call. Nothing enforces that
# but this check: a `usize` in an `extern "C"` signature is an i32 on wasm32
# and an i64 on wasm64, and JS will not convert a Number to a BigInt on its
# own. `gex_alloc` and `gex_free` drifted exactly this way once.
#
# Only runs when the 32-bit shim is present to compare against.
THIRTYTWO=target/wasm32-unknown-unknown/release/gex_browser.wasm
if [ -f "$THIRTYTWO" ] && command -v node >/dev/null 2>&1; then
    node --input-type=module -e "
      import { readFileSync } from 'node:fs';
      import { exportSignatures, importSignatures } from './shims/browser/js/wasm-sig.mjs';
      const T = { 0x7f: 'i32', 0x7e: 'i64', 0x7d: 'f32', 0x7c: 'f64' };
      const f = (s) =>
        '(' + s.params.map((t) => T[t]).join(',') + ')->' +
        (s.results.map((t) => T[t]).join(',') || '()');
      let bad = 0;
      for (const [what, read] of [['export', exportSignatures], ['import', importSignatures]]) {
        const a = read(readFileSync('$THIRTYTWO'));
        const b = read(readFileSync('$OUT'));
        for (const [k, v] of b) {
          const x = a.get(k);
          if (x && f(x) !== f(v)) {
            console.error(\`shims/browser: $what \${k} differs: \${f(x)} on wasm32, \${f(v)} on wasm64\`);
            bad++;
          }
        }
      }
      if (bad) {
        console.error('shims/browser: the two shims do not present the same ABI to JS.');
        console.error('shims/browser: a \`usize\` in an extern \"C\" signature is the usual cause; use u32.');
        process.exit(1);
      }
      console.log('shims/browser: ABI identical to the 32-bit shim');
    "
fi
