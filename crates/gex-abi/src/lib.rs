//! The contract every gex component agrees on.
//!
//! Translated wasm code, the kernel and the host shim all read and write the
//! same guest memory, so the layout of the thread context, the guest address
//! space and the host import surface has to live in exactly one place. That
//! place is here. Nothing in this crate depends on anything else in gex.

#![forbid(unsafe_code)]
#![no_std]

// ---------------------------------------------------------------------------
// Guest address space (PLAN §7.1)
// ---------------------------------------------------------------------------

/// Wasm page size.
pub const WASM_PAGE: u64 = 65536;

/// Size of the guest's linear memory, in bytes.
///
/// One page short of 4 GiB. The missing top page is what makes wild pointers
/// trappable: address computations that overflow 32 bits are steered to
/// [`POISON_ADDR`], which lands in that unmapped page and raises a wasm
/// out-of-bounds trap, which the runtime turns into SIGSEGV.
pub const GUEST_MEM_BYTES: u64 = 0xFFFF_0000;

/// Number of wasm pages in [`GUEST_MEM_BYTES`].
pub const GUEST_MEM_PAGES: u64 = GUEST_MEM_BYTES / WASM_PAGE;

/// Highest guest address that can ever be valid.
pub const GUEST_LIMIT: u64 = GUEST_MEM_BYTES;

/// Address used for effective addresses that do not fit in 32 bits. Chosen so
/// that even a 16-byte access starting here stays out of bounds.
pub const POISON_ADDR: u32 = 0xFFFF_FFF0;

/// Start of the runtime-reserved low region. Page 0 is left unmapped so that
/// guest null-pointer dereferences hit *something* recognisable in a debugger,
/// even though wasm cannot fault on them.
pub const RESERVED_BASE: u64 = 0x1000;

/// End of the runtime-reserved low region; guest mappings start above here.
///
/// Deliberately below the 0x400000 link address of non-PIE aarch64 binaries
/// (see [`EXEC_BASE`]) so a fixed-address image never collides with runtime
/// state. 1 MiB holds ~950 thread contexts, well past any per-process thread
/// count we intend to support.
pub const RESERVED_END: u64 = 0x0010_0000;

/// Default load bias for position-independent executables.
pub const PIE_BASE: u64 = 0x2000_0000;

/// Where a non-PIE image's own `p_vaddr` values are honoured from. Non-PIE
/// aarch64 links at 0x400000, which is above both the reserved region and the
/// dispatch directory, so a fixed-address image never collides with them.
pub const EXEC_BASE: u64 = 0x0040_0000;

/// Top of the guest stack. Grows down from here.
pub const STACK_TOP: u64 = 0x8000_0000;

/// Default guest stack size (8 MiB, matching a typical `RLIMIT_STACK`).
pub const STACK_SIZE: u64 = 8 * 1024 * 1024;

/// Base of the top-down mmap region.
pub const MMAP_TOP: u64 = 0x7000_0000;

/// Lowest address the mmap allocator will hand out.
pub const MMAP_BOTTOM: u64 = 0x3000_0000;

// ---------------------------------------------------------------------------
// Thread context (PLAN §4)
// ---------------------------------------------------------------------------

/// Byte offsets within the per-thread guest context structure.
///
/// The context lives in guest linear memory inside the reserved low region, so
/// translated code reaches it with ordinary loads and stores and the host
/// reaches it by indexing the same memory.
pub mod ctx {
    /// `x0`–`x30`, 8 bytes each.
    pub const X: usize = 0;
    /// Stack pointer.
    pub const SP: usize = 31 * 8;
    /// Program counter. Only meaningful at block boundaries and safepoints.
    pub const PC: usize = 32 * 8;

    /// Condition flags, one 32-bit slot each. Kept unpacked because almost
    /// every use is a single-flag test; `mrs x0, nzcv` reassembles them.
    pub const NF: usize = 33 * 8;
    pub const ZF: usize = NF + 4;
    pub const CF: usize = NF + 8;
    pub const VF: usize = NF + 12;

    /// `TPIDR_EL0` — the guest thread pointer.
    pub const TPIDR: usize = 35 * 8;
    /// `TPIDRRO_EL0`.
    pub const TPIDRRO: usize = 36 * 8;

    /// Local exclusive monitor: address, value and access size. `SIZE == 0`
    /// means the monitor is clear.
    pub const MON_ADDR: usize = 37 * 8;
    pub const MON_VALUE: usize = 38 * 8;
    pub const MON_SIZE: usize = 39 * 8;

    /// FP control and status registers.
    pub const FPCR: usize = 40 * 8;
    pub const FPSR: usize = FPCR + 4;

    /// Per-thread interrupt flag, polled at safepoints for signal delivery.
    pub const INTERRUPT: usize = 41 * 8;
    /// Countdown to the next safepoint poll in tier-1 code.
    pub const SAFEPOINT_COUNTER: usize = 41 * 8 + 4;

    /// Guest thread id, as reported by `gettid`.
    pub const TID: usize = 42 * 8;

    /// `v0`–`v31`, 16 bytes each. Reserved now so the layout does not move
    /// when NEON lands (PLAN M2).
    pub const V: usize = 48 * 8;

    /// Total size of the context structure, rounded up.
    pub const SIZE: usize = V + 32 * 16;

    /// Byte offset of general-purpose register `n` (0..=30).
    #[inline]
    pub const fn x(n: u8) -> usize {
        X + (n as usize) * 8
    }

    /// Byte offset of vector register `n` (0..=31).
    #[inline]
    pub const fn v(n: u8) -> usize {
        V + (n as usize) * 16
    }
}

// ---------------------------------------------------------------------------
// Dispatch (PLAN §5.6)
// ---------------------------------------------------------------------------

/// Bytes of guest text one dispatch-directory entry covers.
pub const DISPATCH_PAGE: u64 = 4096;

/// Dispatch-table slots per covered page: one per instruction slot.
pub const SLOTS_PER_PAGE: u64 = DISPATCH_PAGE / 4;

/// Table slots reserved at index 0, all holding the miss stub.
///
/// This is what makes the lookup branchless. A page with no translation has a
/// directory entry of zero, so the computed index lands inside this reserved
/// span and calls the stub — no comparison against a sentinel, no branch.
pub const MISS_SLOTS: u64 = SLOTS_PER_PAGE;

/// Guest address of the dispatch directory: one `u32` per page of guest text,
/// holding the base table slot for that page, or zero when it has none.
///
/// A flat table indexed by `(pc - base) >> 2` cannot span an address space
/// with libraries mapped far apart — it would need a slot for every four
/// bytes in between. Indirecting through a page directory keeps the table
/// proportional to the code actually mapped, which is what makes dynamic
/// linking affordable.
pub const DISPATCH_DIR: u64 = RESERVED_END;

/// Pages the directory covers, and so the highest address code may occupy.
pub const DISPATCH_MAX_PAGES: u64 = 0x8_0000;

/// Size of the directory in bytes.
pub const DISPATCH_DIR_BYTES: u64 = DISPATCH_MAX_PAGES * 4;

/// First guest address above the dispatch directory.
pub const DISPATCH_DIR_END: u64 = DISPATCH_DIR + DISPATCH_DIR_BYTES;

/// Ceiling on the dispatch table, so it can grow as libraries are mapped.
///
/// Ten million slots is 9,765 covered pages, or 38 MiB of guest text. The
/// table is created at the image's size and grown from there, so the ceiling
/// costs nothing until it is used.
///
/// The number is V8's, not a choice: `WebAssembly.Table.grow` refuses past
/// ten million however the table was declared, so a larger ceiling here was
/// only ever a promise the browser would not keep. It said `1 << 24`, and
/// what a page actually got was `failed to grow table by 20480` at 9,996,288
/// with no indication that the limit was the engine's.
pub const DISPATCH_TABLE_MAX_SLOTS: u64 = 10_000_000;

/// Directory index for a guest address.
#[inline]
pub const fn dispatch_page(pc: u64) -> u64 {
    pc / DISPATCH_PAGE
}

/// Scratch doubleword used to neutralise a store-exclusive whose local
/// monitor no longer matches (see `gex_lift::Lifter::store_exclusive`).
///
/// Placed in the reserved low region, 8-byte aligned, and never mapped into
/// anything the guest can observe.
pub const EXCLUSIVE_SCRATCH: u64 = RESERVED_END - 64;

/// Address of the context for thread slot `n`.
///
/// Contexts are packed into the reserved low region, one per guest thread.
#[inline]
pub const fn ctx_addr(slot: u32) -> u64 {
    RESERVED_BASE + (slot as u64) * ((ctx::SIZE as u64 + 63) & !63)
}

// ---------------------------------------------------------------------------
// Host import surface
// ---------------------------------------------------------------------------

/// Wasm import module name for every host-provided function.
pub const IMPORT_MODULE: &str = "gex";

/// Names of the functions translated code imports from the host.
pub mod imports {
    /// `svc #0`. Signature `(ctx: i32, pc: i64) -> ()`.
    ///
    /// Reads the syscall number and arguments from the context and writes the
    /// result to `x0`. Exiting the guest is signalled by trapping, not by a
    /// return value, so the common path costs one call and no branch.
    pub const SYSCALL: &str = "syscall";

    /// An indirect branch to an address with no translation.
    /// Signature `(ctx: i32, pc: i64) -> i32`, returning the dispatch-table
    /// index to re-enter. In M0 this always traps; from M3 it drives gex-jit.
    pub const DISPATCH_MISS: &str = "dispatch_miss";

    /// A decoded-but-unimplemented or architecturally undefined instruction.
    /// Signature `(ctx: i32, pc: i64, raw: i32) -> ()`. Raises SIGILL.
    pub const ILLEGAL: &str = "illegal";

    /// `brk #imm`. Signature `(ctx: i32, pc: i64, imm: i32) -> ()`.
    pub const BREAKPOINT: &str = "breakpoint";

    /// A system register read the compiler could not resolve statically.
    /// Signature `(ctx: i32, pc: i64, sysreg: i32) -> i64`.
    pub const SYSREG_READ: &str = "sysreg_read";

    /// A system register write. Signature `(ctx: i32, pc: i64, sysreg: i32,
    /// value: i64) -> ()`.
    pub const SYSREG_WRITE: &str = "sysreg_write";

    /// Cache maintenance, including the `IC IVAU` translation-invalidation
    /// hook. Signature `(ctx: i32, pc: i64, op: i32, addr: i64) -> ()`.
    pub const CACHE_OP: &str = "cache_op";

    /// Fused multiply-add: `round(a * b + c)` with one rounding.
    ///
    /// Signature `(ctx: i32, a: i64, b: i64, c: i64, double: i32) -> i64`,
    /// with the operands and result as bit patterns -- single precision in
    /// the low 32 bits. Wasm has no fused multiply-add, and a multiply
    /// followed by an add rounds twice, which is enough to make musl's `pow`
    /// return 1024.0000000000002 for `2^10`. Every host has one, so this asks
    /// rather than approximating.
    pub const FMA: &str = "fma";

    /// Safepoint poll taken when the interrupt flag is set.
    /// Signature `(ctx: i32, pc: i64) -> ()`.
    pub const SAFEPOINT: &str = "safepoint";
}

/// Names translated modules export back to the host.
pub mod exports {
    /// The guest's linear memory.
    pub const MEMORY: &str = "memory";
    /// The PC-indexed dispatch table of translated block entry points.
    pub const DISPATCH_TABLE: &str = "dispatch";
    /// `(ctx: i32, pc: i64) -> ()`: enter translated code at `pc`.
    pub const ENTER: &str = "enter";
}

/// Cache-maintenance operation codes passed to [`imports::CACHE_OP`].
pub mod cache_op {
    /// `dc zva` — zero a cache line's worth of memory.
    pub const DC_ZVA: i32 = 0;
    /// `dc cvau`/`civac`/… — clean; a no-op on a coherent host.
    pub const DC_CLEAN: i32 = 1;
    /// `ic ivau`/`iallu` — invalidate translations covering the address.
    pub const IC_INVALIDATE: i32 = 2;
    /// `isb` — translation barrier.
    pub const ISB: i32 = 3;
    /// Anything else.
    pub const OTHER: i32 = 4;
}

/// Size in bytes zeroed by one `dc zva`, and the value reported by
/// `DCZID_EL0`. 64 bytes is what every mainstream aarch64 core reports.
pub const DCZID_BLOCK_BYTES: u64 = 64;

/// `CTR_EL0` value reported to the guest: 64-byte I- and D-cache lines,
/// with the "instruction cache is not coherent" behaviour that makes guests
/// emit the `IC IVAU` sequences gex relies on for SMC detection (PLAN §6.2).
pub const CTR_EL0: u64 = 0x8444_c004;

/// `CNTFRQ_EL0`: virtual counter frequency in Hz. 1 GHz keeps the guest's
/// arithmetic exact against a nanosecond host clock.
pub const CNTFRQ: u64 = 1_000_000_000;

/// Table slot for a guest address, given its page's directory entry.
#[inline]
pub const fn dispatch_index(pc: u64, page_base: u32) -> u64 {
    page_base as u64 + ((pc % DISPATCH_PAGE) >> 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The context layout is a compile-time contract between codegen, the
    // kernel and the host, so it is checked at compile time.
    const _: () = {
        assert!(ctx::x(0) == 0);
        assert!(ctx::x(30) == 240);
        assert!(ctx::SP > ctx::x(30));
        assert!(ctx::PC > ctx::SP);
        assert!(ctx::NF > ctx::PC);
        assert!(ctx::TPIDR > ctx::VF);
        assert!(ctx::V >= ctx::TID + 8);
        assert!(ctx::SIZE == ctx::v(31) + 16);
        // The vector slots must be 16-byte aligned for `v128` accesses.
        assert!(ctx::V % 16 == 0);
    };

    #[test]
    fn guest_memory_leaves_a_trap_page() {
        assert!(GUEST_MEM_BYTES.is_multiple_of(WASM_PAGE));
        assert!((POISON_ADDR as u64) >= GUEST_MEM_BYTES);
        assert!((POISON_ADDR as u64) + 16 <= u32::MAX as u64 + 1);
    }

    #[test]
    fn layout_regions_are_ordered() {
        assert!(RESERVED_BASE < RESERVED_END);
        // Runtime state and the dispatch directory must both stay clear of
        // the fixed non-PIE link address.
        assert!(RESERVED_END <= DISPATCH_DIR);
        assert!(DISPATCH_DIR_END <= EXEC_BASE);
        // Every address code may occupy must have a directory entry.
        assert!(DISPATCH_MAX_PAGES * DISPATCH_PAGE >= STACK_TOP);
        assert!(EXEC_BASE < PIE_BASE);
        assert!(MMAP_BOTTOM < MMAP_TOP);
        assert!(MMAP_TOP <= STACK_TOP);
        assert!(STACK_TOP < GUEST_LIMIT);
    }
}
