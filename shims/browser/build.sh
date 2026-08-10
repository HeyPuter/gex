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
# What a process's memory may grow to, and what it is allocated with. They
# differ because there is one memory per process and `initial` is paid for the
# moment the process exists -- see `gex_abi::GUEST_MEM_INITIAL_BYTES`.
GUEST_BYTES=$((0xFFFF0000))
GUEST_INITIAL=$((0x05000000))
# The guest's stack tops out at STACK_TOP (0x0400_0000). Above it: 8 MiB of
# shim stack, then the shim's data. The shim's *heap* is not here at all any
# more -- it claims spans from the same growing arena the guest's mappings
# come from, so neither pays for a ceiling it is not using.
SHIM_STACK_TOP=$((0x04800000))
SHIM_BASE=$SHIM_STACK_TOP
LINK="-C link-arg=--import-memory \
  -C link-arg=--initial-memory=$GUEST_INITIAL \
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

# Install it where *everything* loads it from, and check it before anything
# does.
#
# There used to be two copies: `run-node.mjs` read the one in `target/`, the
# page fetched `/gex/gex_browser.wasm` out of `web/public`, and only
# `web/build-assets.sh` copied one to the other. So the debugging harness and
# the demo could be built from different code, and were -- the page ran for
# hours against a shim that a plain `cargo build` had produced without
# `--export=__stack_pointer`, and every boot died with "Cannot set properties
# of undefined (setting 'value')" while every harness run passed. A harness
# that does not test what ships is not a harness.
#
# One artefact now, produced here, and the glue check is part of producing it
# rather than of copying it.
PUB=web/public/gex
mkdir -p "$PUB"
if ! node shims/browser/js/check-glue.mjs "$OUT" shims/browser/js/gex.mjs; then
    echo "shims/browser: NOT installed -- the glue check failed" >&2
    exit 1
fi
# And against the other side. The kernel is a separate artefact that nothing
# here rebuilds, and a shim importing a function it does not export fails to
# instantiate inside a Worker: no error anybody reads, no guest output, a run
# that looks wedged. Adding one kernel function and rebuilding only the shim
# cost exactly that, against a kernel seven hours stale.
if [ -f "$PUB/gex_kernel.plain.wasm" ] \
   && ! node shims/browser/js/check-kernel.mjs "$OUT" "$PUB/gex_kernel.plain.wasm"; then
    echo "shims/browser: NOT installed -- the kernel check failed" >&2
    exit 1
fi
cp "$OUT" "$PUB/gex_browser.wasm"

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
        # Loudly, and with the reason.
        #
        # This used to swallow the output and blame a missing toolchain, and
        # for a while it was wrong: the link was failing on a real error
        # ("initial memory too small") and the message said to install rust-src.
        # A build that reports someone else's problem is worse than one that
        # fails, because the work carried on against a binary that was no
        # longer being produced.
        echo "shims/browser: the shared-memory build FAILED --"
        RUSTFLAGS="$LINK \
            -C link-arg=--shared-memory \
            -C target-feature=+atomics,+bulk-memory,+mutable-globals" \
            cargo +nightly build -p gex-browser --target wasm32-unknown-unknown \
              --release -Z build-std=std,panic_abort --features shared-memory \
              --target-dir target/shared 2>&1 | grep -E "^error|error:" | head -5
        echo "shims/browser: (if that is 'can't find crate for std', install rust-src)"
    fi
fi

# The global kernel too, because the shim imports from it and the two have to
# agree about the *types* of what it imports, not only the names.
#
# `check-kernel.mjs` compares names and passed while `k_obj_translated` had
# gained a second parameter on one side and not the other; the page then died
# at instantiation with `imported function does not match the expected type`.
# Building both from one command is the only thing that makes them agree by
# construction.
if [ -x shims/kernel/build.sh ]; then
    shims/kernel/build.sh >/dev/null 2>&1 && echo "  kernel: rebuilt alongside the shim" \
        || echo "  kernel: NOT rebuilt (needs nightly) -- imports may not match"
fi

# The seed module too, because `enter` lives in it and `enter` is emitted by
# `gex-emit` -- the same crate this shim is built from.
#
# It is produced by `gexc`, not by this script, so it sat a day stale while
# every emitter change of the day went into the shim beside it. A fix to
# `enter`'s dispatch was written, built, and run four times against a seed
# that did not contain it, and the crash came back byte-identical each time
# and was read as the fix not working. `cargo build -p gex-emit` compiles the
# library and not the binary that emits this, so even rebuilding the crate
# does not help; `gexc` has to be rebuilt.
#
# Cheap: the seed translates nothing and is under two kilobytes.
cargo build --release -p gexc 2>&1 | grep -E "^error" && exit 1
target/release/gexc seed -o web/public/gex/seed.wasm --no-shared-memory >/dev/null
target/release/gexc seed -o web/public/gex/seed.shared.wasm >/dev/null
echo "  seed: rebuilt from this emitter"

# And the site, which is a copy of public/ and will otherwise keep serving
# the shim that was here a moment ago.
web/refresh-dist.sh
