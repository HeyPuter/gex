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

/// What a process's memory is *allocated* with, as opposed to what it may
/// grow to.
///
/// Everything below [`MMAP_BOTTOM`] is the fixed layout a process needs
/// before it runs: the register contexts, the dispatch directory, the image,
/// its break and its stack, and the runtime's own stack and data. Above it
/// nothing is touched until something asks, and the memory is grown then.
///
/// This is the number multiplied by the number of processes a run has, so it
/// is the one that decides whether a demo works on somebody else's machine.
pub const GUEST_MEM_INITIAL_BYTES: u64 = MMAP_BOTTOM;

/// Number of wasm pages in [`GUEST_MEM_INITIAL_BYTES`].
pub const GUEST_MEM_INITIAL_PAGES: u64 = GUEST_MEM_INITIAL_BYTES / WASM_PAGE;

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
///
/// Low, because every byte below the highest thing a process touches is
/// memory that process's `WebAssembly.Memory` has to have from the moment it
/// is created -- and there is one of those per process. 16 MiB leaves room
/// under it for a non-PIE image at [`EXEC_BASE`] and the runtime's own
/// tables, and puts the image where a small process's whole world fits in a
/// few tens of megabytes.
pub const PIE_BASE: u64 = 0x0100_0000;

/// Where a non-PIE image's own `p_vaddr` values are honoured from. Non-PIE
/// aarch64 links at 0x400000, which is above both the reserved region and the
/// dispatch directory, so a fixed-address image never collides with them.
pub const EXEC_BASE: u64 = 0x0040_0000;

/// Where a dynamic program's interpreter is placed.
///
/// Above the window the executable and its break share, below the stack. It
/// lives here rather than in a host because the break's ceiling is defined by
/// it: a break that grew past this would run over `ld.so`.
pub const INTERP_BASE: u64 = 0x0300_0000;

/// Highest address the break may reach.
///
/// The executable is loaded at [`PIE_BASE`] and its break grows up from the
/// end of it; 32 MiB is what that window holds, which is more than any
/// program in the suite uses and larger than the biggest binary in it. A
/// program that wants more gets the old break back from `brk`, and libc does
/// what it does on any system that says no: it uses `mmap`.
pub const BRK_TOP: u64 = INTERP_BASE;

/// Top of the guest stack. Grows down from here.
///
/// Just above the image and its break, rather than at the top of the address
/// space. A stack at the top makes the top load-bearing, and a linear memory
/// has to be allocated up to its highest used address: that is how a process
/// came to cost four gigabytes before it had run an instruction, and why the
/// gears demo -- six processes -- asked a browser for twenty-five.
pub const STACK_TOP: u64 = 0x0400_0000;

/// Default guest stack size (8 MiB, matching a typical `RLIMIT_STACK`).
pub const STACK_SIZE: u64 = 8 * 1024 * 1024;

/// Ceiling of the region a guest's mappings and the runtime's own heap grow
/// into.
///
/// Everything below [`MMAP_BOTTOM`] is a fixed layout that a process needs
/// from the start; everything above it is handed out as it is asked for, and
/// the memory is grown to match. So this is a limit rather than a cost.
///
/// It is exactly the dispatch directory's reach, and that is not a
/// coincidence -- it is the constraint. `ld.so` maps a shared library into
/// this arena and then calls into it, so the arena is somewhere *code* lives,
/// and code at an address the directory cannot name can never be translated.
/// This used to be 0xF000_0000, which is 1.75 GiB past the end of the
/// directory, and the consequence was that no dynamically linked program ran
/// under the native host at all: the first call into libc landed on "branch
/// to 0xefe25e80, which is not executable guest memory". The assertion below
/// is what keeps the two numbers together.
pub const MMAP_TOP: u64 = DISPATCH_MAX_PAGES * DISPATCH_PAGE;

/// Lowest address the mmap allocator will hand out, and the bottom of the
/// region that grows.
pub const MMAP_BOTTOM: u64 = 0x0500_0000;

/// The bottom of the region shared objects are placed in.
///
/// A translated block has its guest addresses baked into it, so one process
/// can only use another's translation of a library if both have that library
/// at the same address. Nothing about the guest requires that they differ --
/// `ld.so` reserves each object with `mmap(NULL, ..)` and takes what it is
/// handed -- but a per-process bump allocator gives a different answer
/// depending on what was mapped before it, so the same libc lands at
/// `0x6010000` in one process and `0x6060000` in the next.
///
/// So a window is set aside at a fixed place, and a system-wide table says
/// which object owns which part of it. See `ObjectPlace`.
pub const PLACE_BOTTOM: u64 = MMAP_BOTTOM;

/// How much of the address space that window is.
///
/// It has to be a constant, and it has to be reserved from the start. The
/// runtime's own heap takes from the same monotonic cursor the arena does,
/// so an address the placement table hands out later can be one the heap has
/// already used -- and a placement is only safe if nothing in any process
/// could ever have been put there. There is no way to learn the right size
/// during a run and no way to take it back afterwards.
///
/// The cost is real but smaller than it looks: it is address space, and the
/// memory only grows to what is touched, but [`ARENA_BOTTOM`] sits above the
/// whole window so every process pays all of it once it allocates anything.
/// Against that, the libraries it holds are ones the process was going to map
/// somewhere regardless -- so the true cost is the objects *other* programs
/// placed, and for a pipeline running the same few programs that is nearly
/// nothing.
///
/// Sixty-four megabytes is apt's whole closure with room over. A run whose
/// objects do not fit gets no placement for the overflow, translates them
/// per-process as it did before, and is otherwise unaffected.
pub const PLACE_WINDOW: u64 = 64 << 20;

/// One past the top of the placement window.
pub const PLACE_TOP: u64 = PLACE_BOTTOM + PLACE_WINDOW;

/// Lowest address the growing arena -- guest mappings and the runtime's own
/// heap alike -- will hand out.
///
/// Above the placement window, because the two must never overlap and only
/// this one can be made to move.
pub const ARENA_BOTTOM: u64 = PLACE_TOP;

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

    /// Condition flags: one **byte** each, inside a single 32-bit slot.
    ///
    /// They used to be a 32-bit slot each, "kept unpacked because almost every
    /// use is a single-flag test". The `cmp`+`b.cond` fusion falsified the
    /// premise: a fused branch asks the original operands, so the single-flag
    /// *reads* mostly stopped happening and what is left is the *writes* --
    /// and every flag-setting instruction in the architecture writes all four
    /// at once. `GEX_REG_HISTOGRAM` on gzip puts the four together at 13.9% of
    /// all register traffic, more than any register except X0.
    ///
    /// A byte each rather than a bit each so that nothing regresses: storing
    /// one flag is still one `i32.store8`, exactly as cheap as the separate
    /// slots were, while storing all four is one `i32.store` instead of four.
    /// Bit-packing would have made the single-flag case a read-modify-write.
    ///
    /// Every flag holds 0 or 1 -- the lifter masks with `& 1` on every path
    /// that writes one, and `adc` reads C as a numeric addend -- so a byte and
    /// a 32-bit slot hold exactly the same values.
    pub const NZCV: usize = 33 * 8;
    pub const NF: usize = NZCV;
    pub const ZF: usize = NZCV + 1;
    pub const CF: usize = NZCV + 2;
    pub const VF: usize = NZCV + 3;

    /// Assemble the packed slot from four 0/1 flags, and take it apart.
    ///
    /// The host reaches the flags through these rather than through the byte
    /// offsets, so the packing lives in one place.
    #[inline]
    pub const fn pack_nzcv(n: u32, z: u32, c: u32, v: u32) -> u32 {
        (n & 1) | ((z & 1) << 8) | ((c & 1) << 16) | ((v & 1) << 24)
    }

    /// `(n, z, c, v)`, each 0 or 1.
    #[inline]
    pub const fn unpack_nzcv(packed: u32) -> (u32, u32, u32, u32) {
        (
            packed & 1,
            (packed >> 8) & 1,
            (packed >> 16) & 1,
            (packed >> 24) & 1,
        )
    }

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

    /// A slot nothing reads, for measurement only.
    ///
    /// `GEX_DISPATCH_BALLAST` adds load-and-store pairs to the indirect
    /// dispatch path and puts the loaded value here. A store is the one thing
    /// an engine may not delete -- it cannot prove nobody reads the address --
    /// so this is work that definitely happens, and the slope of run time
    /// against the number of pairs is how much a program cares about work on
    /// that path. See `emit_dispatch_tail_call`.
    pub const BALLAST: usize = 43 * 8;

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

/// How many dispatch tables there are.
///
/// One table cannot hold a real application. The graphics rootfs is 89.5 MiB
/// of guest text across 73 objects -- 46 of it libLLVM, 17 libz3, 15 Mesa's
/// rasteriser -- and a slot per four-byte address makes that 23,457,792
/// slots against V8's ten million, which is 38 MiB of text. Static
/// recompilation of that rootfs is 2.3x over the ceiling, and the
/// `--recompile-objects` cap of 8 MiB that "works" works precisely because it
/// excludes the three big objects.
///
/// wasm allows many tables and `call_indirect` names one, so the ceiling is
/// per table rather than per program. Eight of them is 80,000,000 slots, or
/// 305 MiB of guest text.
///
/// The cost is a `br_table` on the indirect-branch path, and only there:
/// direct branches use `return_call` and never look at a table. Set this to 1
/// and the emitter goes back to a single `return_call_indirect` with no
/// selection at all, byte for byte what it emitted before there were several
/// -- which is what makes the cost of the change measurable rather than
/// argued about.
pub const DISPATCH_TABLES: usize = 8;

/// Bits of a directory entry that hold the slot; the rest hold the table.
///
/// Ten million needs 24 bits, so the top eight of the `u32` were free. That
/// is where the table index goes, which is why several tables cost nothing in
/// the directory -- no second array, no wider entry, no extra load.
pub const DISPATCH_SLOT_BITS: u32 = 24;

/// Mask of the slot half of a directory entry.
pub const DISPATCH_SLOT_MASK: u32 = (1u32 << DISPATCH_SLOT_BITS) - 1;

/// Pack a table index and a base slot into a directory entry.
///
/// Entry zero has to keep meaning "no translation, table 0, the miss span",
/// because that is what an unwritten directory reads as and what makes the
/// lookup branchless.
#[inline]
pub const fn dispatch_entry(table: usize, base: u32) -> u32 {
    ((table as u32) << DISPATCH_SLOT_BITS) | (base & DISPATCH_SLOT_MASK)
}

/// The table a directory entry names.
#[inline]
pub const fn dispatch_entry_table(entry: u32) -> usize {
    (entry >> DISPATCH_SLOT_BITS) as usize
}

/// The base slot a directory entry names, within its own table.
#[inline]
pub const fn dispatch_entry_base(entry: u32) -> u32 {
    entry & DISPATCH_SLOT_MASK
}

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

    /// Not a `brk` at all: the stack-pointer check reporting a wild `sp`.
    ///
    /// Reuses the breakpoint import rather than adding one, so a shim built
    /// before the check existed still links. A guest `brk` immediate is 16
    /// bits unsigned, so a negative value cannot be one and the host can tell
    /// the two apart without ambiguity.
    ///
    /// See `tier1::emit_sp_check`, and #73, where a corrupt `sp` surfaced as
    /// a fault in an innocent function's prologue several calls later.
    pub const BRK_BAD_SP: i32 = -1;

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
    /// Writes one of the module's passive element segments into the tables
    /// it imports, chosen by index. See `Module::add_fill`: a passive segment
    /// costs nothing at instantiation, and instantiation happens once per
    /// guest process, so this is how a module's blocks reach a table without
    /// every process paying for every block.
    pub const FILL: &str = "gexfill";
    /// Writes whichever passive segment holds a given dispatch slot, which is
    /// the question a miss can actually ask. See `Module::add_fill_at`.
    pub const FILL_AT: &str = "gexfillat";
    /// Writes every passive segment in one call, which is what an eager fill
    /// wants: a crossing per segment costs more than the writes.
    pub const FILL_ALL: &str = "gexfillall";

    /// The rest of them. There is more than one because V8 caps a table at
    /// ten million entries and a real application needs more; see
    /// [`super::DISPATCH_TABLES`].
    ///
    /// Table 0 keeps the bare name it always had, so a module built before
    /// there were several still names the same thing -- and so the miss span,
    /// which lives at the bottom of table 0, is where an unwritten directory
    /// entry of zero has always pointed.
    pub const DISPATCH_TABLE_N: [&str; 8] = [
        "dispatch",
        "dispatch1",
        "dispatch2",
        "dispatch3",
        "dispatch4",
        "dispatch5",
        "dispatch6",
        "dispatch7",
    ];

    /// The name of dispatch table `i`.
    pub const fn dispatch_table(i: usize) -> &'static str {
        DISPATCH_TABLE_N[i]
    }

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
///
/// Takes the whole entry, table index and all, and masks: a caller that
/// already had the base can pass it unchanged, because a base is an entry
/// whose table happens to be zero.
#[inline]
pub const fn dispatch_index(pc: u64, page_entry: u32) -> u64 {
    dispatch_entry_base(page_entry) as u64 + ((pc % DISPATCH_PAGE) >> 2)
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
        // Every address code may occupy must have a directory entry. That
        // includes the mmap arena: `ld.so` maps libraries there and calls
        // into them, so a directory that stops below the arena's ceiling
        // leaves executable addresses it cannot name.
        assert!(DISPATCH_MAX_PAGES * DISPATCH_PAGE >= STACK_TOP);
        assert!(DISPATCH_MAX_PAGES * DISPATCH_PAGE >= MMAP_TOP);
        assert!(EXEC_BASE < PIE_BASE);
        assert!(MMAP_BOTTOM < MMAP_TOP);
        // The stack sits at the top of the fixed low region, and the arena
        // starts above it and grows upward.
        assert!(PIE_BASE < INTERP_BASE);
        assert!(INTERP_BASE < STACK_TOP);
        assert!(STACK_TOP <= MMAP_BOTTOM);
        assert!(MMAP_TOP <= GUEST_LIMIT);
    }
}
