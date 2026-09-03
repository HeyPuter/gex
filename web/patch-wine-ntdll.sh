#!/usr/bin/env bash
# Lower Wine's WoW64 "high half" boundary from 4 GiB to 2 GiB in a curated
# Wine rootfs, so 64-bit Wine's thread/kernel stacks land in a range gex can
# address.
#
# Why: gex is wasm32 — guest addresses are direct 32-bit offsets into a linear
# memory that tops out just under 4 GiB, so nothing at or above 4 GiB is
# representable. Wine's unix ntdll hardcodes `limit_4g = 1<<32` and, in
# init_thread_stack(), allocates every thread's kernel stack with
# `limit_low = limit_4g` (map_view then searches only [4 GiB, …)). That search
# is empty under gex, init_thread_stack bails, teb->Tib.StackBase keeps
# init_teb's ~0ul placeholder, and call_init_thunk faults computing
# `Rsp = StackBase - 0x28`.
#
# The fix pairs with gex's MMAP_TOP raised to 3 GiB (DISPATCH_MAX_PAGES in
# crates/gex-abi): lowering limit_4g to 2 GiB puts the high half at [2 GiB,
# 3 GiB) and leaves the low half (32-bit / WoW64) at [0, 2 GiB) — Wine's own
# split, scaled to gex's 3 GiB ceiling.
#
# The patch is a single instruction immediate: every `movabs $0x100000000,%rsi`
# in the unix ntdll is init_thread_stack loading limit_4g into the limit_low
# argument (arg 2 = rsi). `limit_4g` reaches no other function through %rsi, and
# the one `movabs $0x100000000,%rax` is an unrelated struct-field store, so the
# rsi-only pattern hits exactly the three stack allocations and nothing else.
#
# Usage: patch-wine-ntdll.sh <path-to-ntdll.so>   (edits in place; idempotent)
set -euo pipefail

NT="${1:?usage: patch-wine-ntdll.sh <ntdll.so>}"
[ -f "$NT" ] || { echo "not a file: $NT" >&2; exit 1; }

python3 - "$NT" <<'PY'
import sys
p = sys.argv[1]
data = bytearray(open(p, "rb").read())
old = bytes.fromhex("48be" + "0000000001000000")   # movabs $0x100000000,%rsi
new = bytes.fromhex("48be" + "0000008000000000")   # movabs $0x80000000, %rsi
n = 0
i = 0
while True:
    j = data.find(old, i)
    if j < 0:
        break
    data[j:j+len(old)] = new
    n += 1
    i = j + len(new)
already = data.count(bytes.fromhex("48be" + "0000008000000000"))
if n == 0 and already:
    print(f"{p}: already patched ({already} sites)")
elif n == 3:
    open(p, "wb").write(data)
    print(f"{p}: patched limit_4g -> 2 GiB at {n} init_thread_stack sites")
else:
    sys.exit(f"{p}: expected 3 movabs $0x100000000,%rsi sites, found {n} "
             f"(+{already} already patched) — Wine build changed, re-verify")
PY
