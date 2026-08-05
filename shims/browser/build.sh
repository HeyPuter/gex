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
RUSTFLAGS="-C link-arg=--import-memory \
  -C link-arg=--initial-memory=$GUEST_BYTES \
  -C link-arg=--max-memory=$GUEST_BYTES \
  -C link-arg=--global-base=$SHIM_BASE \
  -C link-arg=-zstack-size=8388608 \
  -C link-arg=--export=__stack_pointer" \
  cargo build -p gex-browser --target wasm32-unknown-unknown --release
echo "shims/browser: $(stat -c%s target/wasm32-unknown-unknown/release/gex_browser.wasm) bytes"
