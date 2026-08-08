#!/bin/sh
# Build the browser shim.
#
# The shim shares the guest's memory rather than having one of its own, which
# is what makes a syscall a direct read of the guest's buffers. Three linker
# flags arrange it:
#
#   --import-memory     take the memory from the host instead of defining one
#   --global-base       put this module's data above the guest's world, which
#                       ends at STACK_TOP (0x8000_0000); the guest never maps
#                       above it, so nothing collides
#   -z stack-size       how much of the region the shim's stack gets
#
# The stack is the awkward one: wasm-ld places it from address zero whatever
# --global-base says, which lands it on the guest's image. So __stack_pointer
# is exported and JS moves it into this module's region before the first call
# (see gex.mjs). Nothing has run at that point -- there is no start function --
# so there is nothing on the old stack to lose.
#
# --initial-memory must match what the guest module declares, or the import
# will not link.
set -e
cd "$(dirname "$0")/../.."
GUEST_BYTES=$((0xFFFF0000))
# The guest's world ends at STACK_TOP (0x8000_0000). Above it: 8 MiB of shim
# stack, then the shim's data and heap.
SHIM_STACK_TOP=$((0x80800000))
SHIM_BASE=$SHIM_STACK_TOP
LINK="-C link-arg=--import-memory \
  -C link-arg=--initial-memory=$GUEST_BYTES \
  -C link-arg=--max-memory=$GUEST_BYTES \
  -C link-arg=--global-base=$SHIM_BASE \
  -C link-arg=-zstack-size=8388608 \
  -C link-arg=--export=__stack_pointer"

# GEX_HEAP_COUNTERS=1 builds a shim that tracks how much of its heap is out on
# loan, and reports it through `gex_heap_used_kib` -- which the glue folds into
# the message on an unexpected trap.
#
# Worth having because the heap is a *fixed* region: `--global-base` puts it
# above the guest's world and it runs to the top of a memory that cannot grow,
# so there is a real ceiling (`gex_heap_total_kib`, about 2039 MiB) and no
# symptom when a lift hits it. Recompiling Mesa's software rasteriser is the
# case -- 471,205 blocks, a peak working set of 2109 MiB measured natively --
# and telling that apart from a wild pointer needs a number.
#
# Not the default because counting charges every allocation and free, and the
# runtime translator allocates constantly.
FEATURES=""
[ -n "${GEX_HEAP_COUNTERS:-}" ] && FEATURES="--features heap-counters"

OUT=target/wasm32-unknown-unknown/release/gex_browser.wasm
RUSTFLAGS="$LINK" cargo build -p gex-browser --target wasm32-unknown-unknown --release $FEATURES
echo "shims/browser: $(stat -c%s "$OUT") bytes${FEATURES:+ (heap counters on)}"

# And the same shim against a *shared* memory, when the toolchain can build
# one. Two binaries rather than one because a memory import's `shared` flag is
# part of its type: a module built against a shared memory will not
# instantiate against a plain one, or the other way round, so this is not a
# runtime choice. The page picks, by whether it is cross-origin isolated.
#
# What the shared build is for is threads. A process gets one memory; if that
# memory is a SharedArrayBuffer then each of its *threads* can be its own
# Worker and run at the same time, which is the only way real parallelism
# happens in a tab. Processes still never share memory with each other -- that
# is what stops two of them putting libraries at the same addresses, which is
# what the snapshot switcher died of.
#
# It needs nightly: a Rust module importing a shared memory must be built with
# `+atomics`, and the standard library it links has to have been built the
# same way, which is `-Z build-std`. `GEX_NO_SHARED_MEMORY=1` skips it, and a
# machine without nightly simply does not get the second binary.
if [ -z "${GEX_NO_SHARED_MEMORY:-}" ] && rustup toolchain list 2>/dev/null | grep -q nightly; then
    SHARED=target/wasm32-unknown-unknown/release/gex_browser.shared.wasm
    if RUSTFLAGS="$LINK \
        -C link-arg=--shared-memory \
        -C target-feature=+atomics,+bulk-memory,+mutable-globals" \
        cargo +nightly build -p gex-browser --target wasm32-unknown-unknown \
          --release -Z build-std=std,panic_abort --features shared-memory \
          --target-dir target/shared >/dev/null 2>&1; then
        cp target/shared/wasm32-unknown-unknown/release/gex_browser.wasm "$SHARED"
        echo "shims/browser: $(stat -c%s "$SHARED") bytes (shared memory)"
    else
        echo "shims/browser: no shared-memory build (needs nightly and rust-src)"
    fi
fi
