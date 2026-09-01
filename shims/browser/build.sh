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
GUEST_INITIAL=$((0x07000000))
# The guest's stack tops out at STACK_TOP (0x0580_0000). Above it: 8 MiB of
# shim stack, then the shim's data. The shim's *heap* is not here at all any
# more -- it claims spans from the same growing arena the guest's mappings
# come from, so neither pays for a ceiling it is not using.
SHIM_STACK_TOP=$((0x06000000))
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
# And the same list for the shared build below, which takes `shared-memory`
# as well and so cannot reuse the `--features` string verbatim.
#
# It did not take them at all until now: `GEX_HEAP_COUNTERS=1` built a plain
# shim with counters and a shared shim without, and the page loads the shared
# one -- so the counters were on in the artefact nothing runs and off in the
# artefact everything runs. The build even said "heap counters on", about the
# wrong file.
EXTRA=""
[ -n "${GEX_HEAP_COUNTERS:-}" ] && { FEATURES="--features heap-counters"; EXTRA=",heap-counters"; }

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
# The kernel is a separate artefact and the shim imports from it, so it is
# rebuilt here -- before the check below, which would otherwise be run against
# the previous one and pass on a mismatch that is about to matter.
#
# A failure here stops the build. It used to be swallowed -- output to
# /dev/null, and every outcome reported as "NOT rebuilt (needs nightly)" --
# so a borrow error in the kernel's own source was indistinguishable from a
# machine without a nightly toolchain, and the shim went on being rebuilt
# against a kernel binary from before the change. Six runs were spent
# measuring a fix that was not in the artefact, and one of them was written up
# as having fixed something.
#
# Missing nightly is still only a warning: a machine without it should get
# every demo that does not fork. Anything else is an error with the
# compiler's own words under it.
if [ -x shims/kernel/build.sh ]; then
    if ! rustup toolchain list 2>/dev/null | grep -q nightly; then
        echo "  kernel: NOT rebuilt (no nightly toolchain) -- imports may not match"
    elif kerr=$(shims/kernel/build.sh 2>&1); then
        echo "  kernel: rebuilt alongside the shim"
    else
        echo "$kerr" | grep -E "^(error|warning: unused)" -A 6 || echo "$kerr" | tail -20
        echo "  kernel: FAILED to build -- the shim would import a stale one" >&2
        exit 1
    fi
fi
# And against the other side. A shim importing a function the kernel does not
# export -- or exports with a different signature -- fails to instantiate
# inside a Worker: no error anybody reads, no guest output, a run that looks
# wedged. Adding one kernel function and rebuilding only the shim cost exactly
# that, against a kernel seven hours stale; giving one an extra parameter cost
# it again, against a check that compared names only.
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
          --release -Z build-std=std,panic_abort --features "shared-memory$EXTRA" \
          --target-dir target/shared >/dev/null 2>&1; then
        cp target/shared/wasm32-unknown-unknown/release/gex_browser.wasm "$SHARED"
        # And where the page can fetch it.
        #
        # It was built and left in `target/` for as long as the shared build
        # has existed, so the one artifact that lets a guest thread be a
        # Worker was the one artifact the site could not load. The page ran
        # every thread inline instead and nothing said so.
        cp "$SHARED" "$PUB/gex_browser.shared.wasm"
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
              --release -Z build-std=std,panic_abort --features "shared-memory$EXTRA" \
              --target-dir target/shared 2>&1 | grep -E "^error|error:" | head -5
        echo "shims/browser: (if that is 'can't find crate for std', install rust-src)"
    fi
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
