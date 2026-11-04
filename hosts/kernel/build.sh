#!/bin/sh
# Build the global kernel module.
#
# Its own memory is imported, because the host creates it: one shared memory
# for every Worker of every process, which is what makes the file table and
# the process table simply *there* rather than something to message about.
#
# The process's memory is imported too, as memory 1, and nothing in the Rust
# toolchain can say so -- wasm-ld emits exactly one memory and there is no
# attribute for a second. So it is appended after linking; see
# hosts/kernel/add-memory.mjs, which also compiles the result to prove the surgery
# was right.
#
# Nightly, for `asm_experimental_arch`: `memory.copy` naming two memories has
# no spelling in Rust and is written as inline assembly in `gex-kmem`.
set -e
cd "$(dirname "$0")/../.."
# `--shared-memory` and `+atomics`, because the memory this imports is a
# `SharedArrayBuffer` and a memory import's `shared` flag is part of its type:
# a module built without it will not accept one, whatever its size. That is
# also what makes the atomics in here real -- the counter and the note are
# written by different Workers at the same time.
# `GEX_ARCH=x86` builds the kernel for an x86-64 guest (syscall ABI, struct
# layouts, signal frame) into `.x86`-infixed artefacts beside the AArch64 ones.
ARCH_FEATURES=""
INFIX=""
TDIR=target/shared-kernel
if [ "${GEX_ARCH:-}" = "x86" ]; then
    ARCH_FEATURES="--features x86"
    INFIX=".x86"
    TDIR=target/shared-kernel-x86
fi
# What this is built from, taken now, before the build: written beside it as
# page/public/gex/.inputs once it is installed (page/build-inputs.mjs).
INPUTS_KEY=$(node page/build-inputs.mjs key gex_kernel$INFIX)
OUT=$TDIR/wasm32-unknown-unknown/release/gex_kernel_wasm.wasm
# A shared memory carries its maximum in its *type*, so the host's memory and
# this module must agree on it exactly. The linker defaults the maximum to the
# initial, which would freeze the kernel at its own data size; this is the
# ceiling on everything it will ever hold -- every process's file table, every
# pipe's buffer, every byte of every tmpfs.
#
# Four gigabytes, which is all a wasm32 memory can be. It was one, and a
# complete Debian arm64 rootfs did not fit: the rootfs is a tmpfs and a tmpfs
# lives here, so the ceiling on this memory is the ceiling on how large a
# machine gex can boot. The failure gave nothing to go on -- a bare
# `unreachable` out of `rust_oom` -- and it is why `apt-get update` ran on the
# native host and not in a browser. That is #91.
#
# It is a reservation, not an allocation: the host still creates the memory at
# 128 MiB and it grows as files arrive.
# No build host's paths in the kernel (panic locations): fixed names instead.
REMAP="--remap-path-prefix=$(pwd)=/gex --remap-path-prefix=$(rustc +nightly --print sysroot)=/rust --remap-path-prefix=${CARGO_HOME:-$HOME/.cargo}=/cargo"
RUSTFLAGS="$REMAP -C link-arg=--import-memory \
  -C link-arg=--shared-memory \
  -C link-arg=--max-memory=4294967296 \
  -C link-arg=--export-table \
  -C link-arg=--export=__stack_pointer \
  -C link-arg=--export=__wasm_init_tls \
  -C link-arg=--export=__tls_size \
  -C link-arg=--export=__tls_align \
  -C target-feature=+atomics,+bulk-memory,+mutable-globals" \
  cargo +nightly build -p gex-kernel-wasm --target wasm32-unknown-unknown \
    --release -Z build-std=std,panic_abort --target-dir $TDIR $ARCH_FEATURES
# Twice, differing only in whether the *process* memory it reaches into is
# shared. Its own memory is always shared -- that is the point of it. But a
# process is shared only between its threads, and a process that is one Worker
# has a plain memory; a memory import's `shared` flag is part of its type, so
# which one this module declares is not a runtime choice. The host picks the
# file that matches the processes it is running.
# One page as the declared minimum for the *process* memory, not the guest's
# 1280.
#
# A memory import's minimum is a requirement on whoever supplies it, and this
# module has no such requirement: it never assumes a size, it asks with
# `memory.size 1` and refuses an address past the end (`gex_kmem::in_process`).
# Declaring the guest's initial size therefore constrained hosts that are not
# guests -- and the page is one. It instantiates this module too, so that a
# keystroke reaches the line discipline and the process table by a call rather
# than by a message to a Worker; it has no address space of its own and offers
# a page of scratch. `memory import has 1 pages which is smaller than the
# declared initial of 1280` was the whole of why the shared terminal never
# opened, and the page fell back to editing lines itself -- which looks almost
# right, because the only thing the fallback cannot do is `^C`.
#
# A Worker still hands over the guest's memory, which is far larger than one
# page and satisfies the minimum as it always did.
#
# Then a second one, `process64`, a memory64 (gex-kmem's memory 2): one module
# serves processes of both flavours, a 32-bit memory and a 64-bit one,
# each Worker giving its own as the one of its type
# and an empty memory as the other (lazy-fetch.mjs `processMemories`). Two
# builds, one per flavour, laid their statics out 16 bytes apart and could
# not share one kernel memory: in a page whose processes differ (64-bit Wine's
# wasm32 server beside a 64-bit memory's loader, or the X server under
# `?mem64=1`), the second to start read the first's state wrong. Minimum 0
# for both: the empty one has no pages.
P64=$(((16 << 30) / 65536))
ADD_MEMORY_MORE=1 node hosts/kernel/add-memory.mjs "$OUT" "$OUT.p32" env process 0 1
node hosts/kernel/add-memory.mjs "$OUT.p32" page/public/gex/gex_kernel$INFIX.wasm env process64 0 1 $P64 64
ADD_MEMORY_MORE=1 node hosts/kernel/add-memory.mjs "$OUT" "$OUT.p32" env process 0 0
node hosts/kernel/add-memory.mjs "$OUT.p32" page/public/gex/gex_kernel$INFIX.plain.wasm env process64 0 0 $P64 64
rm -f "$OUT.p32"

# The `.m64` name a 64-bit memory's page loads (runtime.js, run-node.mjs):
# the same module, since it serves both flavours.
if [ -n "$ARCH_FEATURES" ]; then
    cp page/public/gex/gex_kernel$INFIX.wasm page/public/gex/gex_kernel$INFIX.m64.wasm
fi

node page/build-inputs.mjs stamp page/public/gex "gex_kernel$INFIX=$INPUTS_KEY"

# And the site, which is a copy of public/ and will otherwise keep serving
# the module that was here a moment ago.
page/refresh-dist.sh
