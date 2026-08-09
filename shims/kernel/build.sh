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
# tools/add-memory.mjs, which also compiles the result to prove the surgery
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
OUT=target/shared-kernel/wasm32-unknown-unknown/release/gex_kernel_wasm.wasm
# A shared memory carries its maximum in its *type*, so the host's memory and
# this module must agree on it exactly. The linker defaults the maximum to the
# initial, which would freeze the kernel at its own data size; 1 GiB is the
# ceiling on everything it will ever hold -- every process's file table, every
# pipe's buffer, every byte of every tmpfs.
RUSTFLAGS="-C link-arg=--import-memory \
  -C link-arg=--shared-memory \
  -C link-arg=--max-memory=1073741824 \
  -C link-arg=--export-table \
  -C link-arg=--export=__stack_pointer \
  -C target-feature=+atomics,+bulk-memory,+mutable-globals" \
  cargo +nightly build -p gex-kernel-wasm --target wasm32-unknown-unknown \
    --release -Z build-std=std,panic_abort --target-dir target/shared-kernel
# Twice, differing only in whether the *process* memory it reaches into is
# shared. Its own memory is always shared -- that is the point of it. But a
# process is shared only between its threads, and a process that is one Worker
# has a plain memory; a memory import's `shared` flag is part of its type, so
# which one this module declares is not a runtime choice. The host picks the
# file that matches the processes it is running.
node tools/add-memory.mjs "$OUT" web/public/gex/gex_kernel.wasm env process 65535 1
node tools/add-memory.mjs "$OUT" web/public/gex/gex_kernel.plain.wasm env process 65535 0
