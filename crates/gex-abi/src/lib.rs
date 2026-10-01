//! The contract every gex component agrees on: the layout of the thread
//! context, the guest address space and the host import surface, in one place.
//! Nothing in this crate depends on anything else in gex.

#![forbid(unsafe_code)]
#![no_std]

// ---------------------------------------------------------------------------
// Guest architecture
// ---------------------------------------------------------------------------

/// True when this build targets an x86-64 guest, false for AArch64. Chosen by
/// the `x86` cargo feature: the two guests differ in constants the emitter,
/// kernel and shims bake into code (instruction alignment, syscall registers),
/// so one binary serves one guest.
pub const X86: bool = cfg!(feature = "x86");

/// True when this build targets a 32-bit big-endian PowerPC guest (the `ppc`
/// cargo feature; docs/ppc.md).
pub const PPC: bool = cfg!(feature = "ppc");

#[cfg(all(feature = "x86", feature = "ppc"))]
compile_error!("gex-abi: the `x86` and `ppc` guests are mutually exclusive");

/// The guest architecture's name as Linux spells it (`uname -m`, `AT_PLATFORM`).
pub const ARCH_NAME: &str = if X86 {
    "x86_64"
} else if PPC {
    "ppc"
} else {
    "aarch64"
};

/// The ELF `e_machine` of a guest object.
pub const ELF_MACHINE: u16 = if X86 {
    62
} else if PPC {
    20
} else {
    183
};

// ---------------------------------------------------------------------------
// Guest byte order
// ---------------------------------------------------------------------------

/// True when the guest is big-endian. Guest memory holds its bytes in the
/// guest's order, so a multi-byte guest value is swapped on its way in and
/// out; the runtime's own structures below [`GUEST_ORDER_FROM`] are always
/// little-endian, since translated code reads them with native wasm loads.
pub const BIG_ENDIAN: bool = PPC;

/// The lowest address holding guest-ordered data. Everything below (thread
/// contexts, the kernel's per-thread scratch, the dispatch directory) is the
/// runtime's and little-endian; no guest mapping is ever placed there (the
/// kernel's `mem::hits_runtime` refuses it), so an address says which order
/// its bytes are in.
pub const GUEST_ORDER_FROM: u64 = DISPATCH_DIR_END;

/// Whether the bytes at `addr` are in big-endian (guest) order.
#[inline(always)]
pub const fn is_guest_be(addr: u64) -> bool {
    BIG_ENDIAN && addr >= GUEST_ORDER_FROM
}

/// Conversion between a guest value and the bytes guest memory holds for it,
/// in the guest's byte order. With a little-endian guest these are exactly
/// `to_le_bytes`/`from_le_bytes`.
pub trait GuestEndian: Sized + Copy {
    type Bytes;
    fn to_ge_bytes(self) -> Self::Bytes;
    fn from_ge_bytes(b: Self::Bytes) -> Self;
}

macro_rules! guest_endian {
    ($($t:ty),*) => {$(
        impl GuestEndian for $t {
            type Bytes = [u8; core::mem::size_of::<$t>()];
            #[inline(always)]
            fn to_ge_bytes(self) -> Self::Bytes {
                if BIG_ENDIAN { self.to_be_bytes() } else { self.to_le_bytes() }
            }
            #[inline(always)]
            fn from_ge_bytes(b: Self::Bytes) -> Self {
                if BIG_ENDIAN { <$t>::from_be_bytes(b) } else { <$t>::from_le_bytes(b) }
            }
        }
    )*};
}
guest_endian!(u16, i16, u32, i32, u64, i64);

/// Log2 of the guest's instruction alignment: A64 instructions are 4-byte
/// aligned words, x86-64 instructions start on any byte.
pub const INST_ALIGN_SHIFT: u32 = if X86 { 0 } else { 2 };

/// The guest's instruction alignment in bytes.
pub const INST_ALIGN: u64 = 1 << INST_ALIGN_SHIFT;

/// `log2` of the dispatch slot granularity: a slot covers this many bytes of
/// guest code. AArch64 instructions are 4-byte aligned, so a slot per
/// instruction. x86-64 instructions start at any byte, but a slot per byte made
/// the dispatch tables (one copy per thread Worker) four times the size and
/// broke the browser's memory budget; a slot per 4 bytes instead, with an
/// in-module trampoline wherever two blocks start in one slot (see
/// `ctx::LAST_TARGET`, which every dispatch records so the trampoline can tell).
pub const DISPATCH_SHIFT: u32 = 2;

/// Length in bytes of the guest's system-call instruction (`svc #0` is a
/// 4-byte word; `syscall` is `0f 05`). Where a syscall resumes is the
/// instruction after it, and a host that restarts or skips one needs this.
pub const SYSCALL_INST_BYTES: u64 = if X86 { 2 } else { 4 };

// ---------------------------------------------------------------------------
// Guest address space (PLAN §7.1)
// ---------------------------------------------------------------------------

/// Wasm page size.
pub const WASM_PAGE: u64 = 65536;

/// Size of the guest's linear memory, in bytes: one page short of 4 GiB. The
/// missing top page makes wild pointers trappable — 32-bit-overflowing
/// addresses are steered to [`POISON_ADDR`] there, which faults as SIGSEGV.
pub const GUEST_MEM_BYTES: u64 = 0xFFFF_0000;

/// Number of wasm pages in [`GUEST_MEM_BYTES`].
pub const GUEST_MEM_PAGES: u64 = GUEST_MEM_BYTES / WASM_PAGE;

/// What a process's memory is *allocated* with, as opposed to what it may grow
/// to. Everything below [`MMAP_BOTTOM`] is the fixed layout a process needs
/// before it runs; above it, memory is grown on demand. Paid per process, so
/// this number decides whether a demo fits on someone else's machine.
pub const GUEST_MEM_INITIAL_BYTES: u64 = MMAP_BOTTOM;

/// Number of wasm pages in [`GUEST_MEM_INITIAL_BYTES`].
pub const GUEST_MEM_INITIAL_PAGES: u64 = GUEST_MEM_INITIAL_BYTES / WASM_PAGE;

/// Highest guest address that can ever be valid.
pub const GUEST_LIMIT: u64 = GUEST_MEM_BYTES;

/// Address used for effective addresses that do not fit in 32 bits. Chosen so
/// that even a 16-byte access starting here stays out of bounds.
pub const POISON_ADDR: u32 = 0xFFFF_FFF0;

/// Start of the runtime-reserved low region. Page 0 is left unmapped so guest
/// null-pointer dereferences land somewhere recognisable in a debugger.
pub const RESERVED_BASE: u64 = 0x1000;

/// End of the runtime-reserved low region; guest mappings start above here.
///
/// Below the 0x400000 non-PIE link address (see [`EXEC_BASE`]) so a
/// fixed-address image never collides with runtime state. 1 MiB holds ~950
/// thread contexts.
pub const RESERVED_END: u64 = 0x0010_0000;

/// Default load bias for position-independent executables.
///
/// Low, because memory up to the highest touched address must exist per
/// process from creation. 16 MiB leaves room under it for a non-PIE image at
/// [`EXEC_BASE`] and the runtime's tables.
pub const PIE_BASE: u64 = 0x0100_0000;

/// Where a non-PIE image's own `p_vaddr` values are honoured from. Non-PIE
/// aarch64 links at 0x400000, which is above both the reserved region and the
/// dispatch directory, so a fixed-address image never collides with them.
pub const EXEC_BASE: u64 = 0x0040_0000;

/// Where a dynamic program's interpreter is placed: above the executable and
/// its break, below the stack. It defines the break's ceiling ([`BRK_TOP`]),
/// since a break growing past it would run over `ld.so`.
pub const INTERP_BASE: u64 = 0x04a0_0000;

/// Highest address the break may reach. The break grows up from the end of
/// the image at [`PIE_BASE`]; a program that wants more gets a failing `brk`
/// and libc falls back to `mmap`.
pub const BRK_TOP: u64 = INTERP_BASE;

/// Top of the guest stack. Grows down from here.
///
/// Kept just above the image and its break, not at the top of the address
/// space: linear memory must be allocated up to its highest used address, so
/// a stack at the top would make every process cost the whole 4 GiB.
pub const STACK_TOP: u64 = 0x0580_0000;

/// Default guest stack size (8 MiB, matching a typical `RLIMIT_STACK`).
pub const STACK_SIZE: u64 = 8 * 1024 * 1024;

/// Ceiling of the region a guest's mappings and the runtime's heap grow into.
///
/// Exactly the dispatch directory's reach, and that is the constraint, not a
/// coincidence: `ld.so` maps libraries into this arena and calls into them, so
/// code lives here, and code the directory cannot name can never be
/// translated. The assertion below keeps the two numbers together.
pub const MMAP_TOP: u64 = DISPATCH_MAX_PAGES * DISPATCH_PAGE;

/// Lowest address the mmap allocator will hand out, and the bottom of the
/// region that grows.
pub const MMAP_BOTTOM: u64 = 0x0700_0000;

/// The bottom of the region shared objects are placed in.
///
/// A translated block has its guest addresses baked in, so processes can share
/// a library's translation only if both map it at the same address. This fixed
/// window plus a system-wide ownership table (`ObjectPlace`) gives every
/// process the same placement, which a per-process bump allocator cannot.
pub const PLACE_BOTTOM: u64 = MMAP_BOTTOM;

/// How much of the address space that window is.
///
/// A fixed constant reserved from the start: the heap and arena share one
/// monotonic cursor, so a placement is only safe if nothing could ever have
/// been put there, and the size cannot be learned during a run or reclaimed
/// after. 64 MiB is apt's whole closure with room over; objects that overflow
/// it fall back to per-process translation.
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

/// Byte offsets within the per-thread guest context structure, which lives in
/// guest linear memory in the reserved low region.
pub mod ctx {
    /// `x0`–`x30`, 8 bytes each.
    pub const X: usize = 0;
    /// Stack pointer. On x86-64 the stack pointer is an ordinary GPR (`rsp`,
    /// `x(4)`), so this names that slot and the kernel's every use of `SP`
    /// reads the right register.
    pub const SP: usize = if super::X86 {
        x(4)
    } else if super::PPC {
        x(1)
    } else {
        31 * 8
    };
    /// Program counter. Only meaningful at block boundaries and safepoints.
    pub const PC: usize = 32 * 8;

    /// Condition flags: one **byte** each, inside a single 32-bit slot. One
    /// flag stores as one `i32.store8`, all four as one `i32.store`; a byte
    /// (not a bit) each keeps the single-flag store off a read-modify-write.
    /// Every flag holds 0 or 1 (the lifter masks `& 1`; `adc` reads C as an
    /// addend).
    pub const NZCV: usize = 33 * 8;
    pub const NF: usize = NZCV;
    pub const ZF: usize = NZCV + 1;
    pub const CF: usize = NZCV + 2;
    pub const VF: usize = NZCV + 3;

    /// x86-64 only: the parity, auxiliary-carry and direction flags, bytes in
    /// the doubleword after NZCV so all six arithmetic flags (N=SF, Z, C,
    /// V=OF, P, A) store as one 64-bit word. `DF` is not an arithmetic flag
    /// (only `cld`/`std` write it) but keeps a byte here for one packed frame.
    pub const PF: usize = NZCV + 4;
    pub const AF: usize = NZCV + 5;
    pub const DF: usize = NZCV + 6;

    /// Pack four 0/1 flags into the slot (and [`unpack_nzcv`] to reverse), so
    /// the packing lives in one place.
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

    /// `TPIDR_EL0` — the guest thread pointer. On x86-64 this slot is the
    /// `FS` segment base, which is how that ABI reaches thread-local storage.
    pub const TPIDR: usize = 35 * 8;
    /// `TPIDRRO_EL0`; the `GS` base on x86-64.
    pub const TPIDRRO: usize = 36 * 8;
    /// The x86-64 names for the same two slots.
    pub const FS_BASE: usize = TPIDR;
    pub const GS_BASE: usize = TPIDRRO;

    /// Local exclusive monitor: address, value and access size. `SIZE == 0`
    /// means the monitor is clear.
    pub const MON_ADDR: usize = 37 * 8;
    pub const MON_VALUE: usize = 38 * 8;
    pub const MON_SIZE: usize = 39 * 8;

    /// FP control and status registers. On x86-64 `FPCR` holds `MXCSR`.
    pub const FPCR: usize = 40 * 8;
    pub const FPSR: usize = FPCR + 4;
    pub const MXCSR: usize = FPCR;

    /// Per-thread interrupt flag, polled at safepoints for signal delivery.
    pub const INTERRUPT: usize = 41 * 8;
    /// Countdown to the next safepoint poll in tier-1 code.
    pub const SAFEPOINT_COUNTER: usize = 41 * 8 + 4;

    /// Guest thread id, as reported by `gettid`.
    pub const TID: usize = 42 * 8;

    /// A slot nothing reads, for measurement only. `GEX_DISPATCH_BALLAST`
    /// writes load/store pairs here on the indirect dispatch path; a store
    /// cannot be optimised away, so it measures work on that path. See
    /// `emit_dispatch_tail_call`.
    pub const BALLAST: usize = 43 * 8;

    /// The target of the last indirect branch, written just before the
    /// `br_table` that dispatches it. Dispatch is a tail call that leaves no
    /// wasm frames, so this is the only record of where a crash on that path
    /// was headed. One store per indirect branch; direct branches use
    /// `return_call` and never come here.
    pub const LAST_TARGET: usize = 44 * 8;
    /// The start of the block that made the last indirect branch, stored
    /// beside `LAST_TARGET`: a wild target is only half the story, the code
    /// that computed it is the other half.
    pub const LAST_SOURCE: usize = 46 * 8;

    /// The raw system-call number the guest last issued, saved by the kernel
    /// at entry. On x86-64 the number's register (`rax`) is also the result
    /// register, so a host that wants to know which call just ran after the
    /// fact cannot read it back from there.
    pub const LAST_SYSCALL: usize = 45 * 8;

    /// `v0`–`v31`, the SIMD/FP register file, 16 bytes each. On x86-64 the
    /// first sixteen are `xmm0`–`xmm15`.
    pub const V: usize = 48 * 8;

    /// x86-64 only: the x87 register stack, eight 16-byte slots holding each
    /// physical register `R(i)` (not stack-relative) in the 80-bit extended
    /// format, laid out as `fxsave` does: the 64-bit significand with its
    /// explicit integer bit at +0, the sign and exponent word at +8, six
    /// bytes of padding.
    pub const X87: usize = V + 32 * 16;
    /// x87 `TOP` (0..7), the physical register `ST(0)` names.
    pub const X87_TOP: usize = X87 + 8 * 16;
    /// x87 control word, as `fldcw`/`fnstcw` see it. Each of these x87
    /// words has a 4-byte slot of its own: translated code moves them as
    /// 32-bit values.
    pub const X87_CW: usize = X87_TOP + 4;
    /// x87 status word, less `TOP` (which lives in `X87_TOP`).
    pub const X87_SW: usize = X87_CW + 4;
    /// One bit per physical register: 1 means valid (tag word derived).
    pub const X87_TAGS: usize = X87_SW + 4;
    /// 16 bytes through which an x87 memory operand passes to and from the
    /// host, which computes the x87's arithmetic (`gex_kernel::f80`): raw
    /// bits at +0, and for the 80-bit format the sign and exponent at +8.
    pub const X87_TMP: usize = X87_TAGS + 4;

    /// A 64-byte per-thread scratch area the kernel may use for structures it
    /// must materialise in guest memory on the guest's behalf (an x86-64
    /// `poll` becomes `ppoll` with a timespec that has to live somewhere).
    pub const SCRATCH: usize = X87_TMP + 16;

    /// PowerPC only: the area after `v31` (where x86-64 keeps its x87
    /// stack). The 32 condition-register bits, one byte each: bit `4f + k` is
    /// field `f`'s LT, GT, EQ, SO for `k` = 0..3.
    pub const PPC_CR: usize = V + 32 * 16;
    /// Link and count registers, 32 bits each.
    pub const PPC_LR: usize = PPC_CR + 32;
    pub const PPC_CTR: usize = PPC_CR + 36;
    /// XER's summary overflow, overflow and carry, one byte each (0 or 1),
    /// and its byte count (bits 25–31, for `lswx`/`stswx`).
    pub const PPC_SO: usize = PPC_CR + 40;
    pub const PPC_OV: usize = PPC_CR + 41;
    pub const PPC_CA: usize = PPC_CR + 42;
    pub const PPC_XER_BC: usize = PPC_CR + 43;
    /// The FPSCR, 32 bits.
    pub const PPC_FPSCR: usize = PPC_CR + 44;
    /// r3 as a system call found it, for restarting one (Linux's `orig_gpr3`).
    pub const PPC_ORIG_R3: usize = PPC_CR + 48;
    /// PowerPC's FPRs are `v0`–`v31`'s low eight bytes, as f64 bit patterns.
    pub const fn f(n: u8) -> usize {
        v(n)
    }

    /// Total size of the context structure, rounded up. The x87 stack and
    /// the scratch area are x86-64's, and PowerPC's registers and scratch
    /// share that space; an AArch64 context ends with `v31`.
    pub const SIZE: usize = if super::X86 || super::PPC {
        SCRATCH + 64
    } else {
        V + 32 * 16
    };

    /// Where the guest puts a system-call number: `x8`, or `rax` (`x(0)` in
    /// gex's numbering of x86-64 registers, which is the hardware's), or
    /// PowerPC's `r0`.
    pub const SYSCALL_NR: usize = if super::X86 || super::PPC { x(0) } else { x(8) };
    /// Where the six system-call arguments arrive: `x0`–`x5`, or
    /// `rdi, rsi, rdx, r10, r8, r9`, or `r3`–`r8`.
    pub const SYSCALL_ARGS: [usize; 6] = if super::X86 {
        [x(7), x(6), x(2), x(10), x(8), x(9)]
    } else if super::PPC {
        [x(3), x(4), x(5), x(6), x(7), x(8)]
    } else {
        [x(0), x(1), x(2), x(3), x(4), x(5)]
    };
    /// Where a system call's result goes: `x0` (`rax` is `x(0)`), or `r3`.
    pub const SYSCALL_RET: usize = if super::PPC { x(3) } else { x(0) };

    /// The first argument register of the guest's C calling convention
    /// (`x0`, or `rdi`, or `r3`), and the next two. What a library-function
    /// intrinsic reads its arguments from.
    pub const CALL_ARGS: [usize; 3] = if super::X86 {
        [x(7), x(6), x(2)]
    } else if super::PPC {
        [x(3), x(4), x(5)]
    } else {
        [x(0), x(1), x(2)]
    };

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

/// Dispatch-table slots per covered page: one per possible instruction start,
/// so 1,024 for A64's 4-byte words and 4,096 for byte-aligned x86-64.
pub const SLOTS_PER_PAGE: u64 = DISPATCH_PAGE >> DISPATCH_SHIFT;

/// Table slots reserved at index 0, all holding the miss stub. An untranslated
/// page has directory entry zero, so its index lands in this span and calls
/// the stub with no sentinel comparison — this is what makes lookup branchless.
pub const MISS_SLOTS: u64 = SLOTS_PER_PAGE;

/// Guest address of the dispatch directory: one `u32` per page of guest text,
/// holding the base table slot for that page, or zero when it has none.
///
/// Indirecting through a page directory keeps the table proportional to the
/// code actually mapped, rather than to the span between far-apart libraries,
/// which is what makes dynamic linking affordable.
pub const DISPATCH_DIR: u64 = RESERVED_END;

/// Pages the directory covers, and so the highest address code may occupy.
pub const DISPATCH_MAX_PAGES: u64 = 0x8_0000;

/// Size of the directory in bytes.
pub const DISPATCH_DIR_BYTES: u64 = DISPATCH_MAX_PAGES * 4;

/// First guest address above the dispatch directory.
pub const DISPATCH_DIR_END: u64 = DISPATCH_DIR + DISPATCH_DIR_BYTES;

/// Ceiling on one dispatch table (10M slots ≈ 38 MiB of guest text). The
/// table grows from the image's size, so the ceiling costs nothing until used.
///
/// The number is V8's, not a choice: `WebAssembly.Table.grow` refuses past ten
/// million however the table is declared, so a larger ceiling would not hold.
pub const DISPATCH_TABLE_MAX_SLOTS: u64 = 10_000_000;

/// How many dispatch tables there are.
///
/// One table (V8's 10M-slot cap, ≈38 MiB of text) cannot hold a real
/// application. wasm allows many tables and `call_indirect` names one, so the
/// cap is per table: eight of them is 305 MiB of guest text.
///
/// The cost is a `br_table` on the indirect-branch path only; direct branches
/// use `return_call`. Set this to 1 and the emitter goes back to a single
/// `return_call_indirect` with no selection at all.
pub const DISPATCH_TABLES: usize = 8;

/// Bits of a directory entry that hold the slot; the top 8 hold the table
/// index. Ten million needs 24 bits, so the table index is free in the `u32`.
pub const DISPATCH_SLOT_BITS: u32 = 24;

/// Mask of the slot half of a directory entry.
pub const DISPATCH_SLOT_MASK: u32 = (1u32 << DISPATCH_SLOT_BITS) - 1;

/// Blocks above which a module's element segments are passive.
///
/// A module past this size gets passive segments and a fill helper, because
/// writing every block into every process's table is most of what a process
/// costs; below it, segments are active and instantiation does the writing.
/// Both emitter and host consult this: active-segment modules keep the
/// closure's numbering, only demand-filled ones move.
pub const BIG_BLOCKS: usize = 4096;

/// Slots a dispatch fill writes around the one that missed, rather than the
/// whole 1,024-entry page (`table.init` materialises a `funcref` per entry,
/// costly on the first fill). Trades entries for misses; wins unless a page is
/// entered at more than `1024 / FILL_SPAN` scattered points.
pub const FILL_SPAN: u32 = 32;

/// Pack a table index and a base slot into a directory entry. Entry zero must
/// keep meaning "no translation, table 0, miss span" — what an unwritten
/// directory reads as, and what makes the lookup branchless.
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

/// Scratch doubleword used to neutralise a store-exclusive whose local monitor
/// no longer matches (see `gex_lift::Lifter::store_exclusive`). In the reserved
/// low region, 8-byte aligned, never guest-observable.
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
    /// `svc #0`. Signature `(ctx: i32, pc: i64) -> ()`. Reads the number and
    /// args from the context, writes the result to `x0`. Guest exit is
    /// signalled by trapping, not a return value.
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

    /// Not a `brk`: the stack-pointer check reporting a wild `sp`, reusing the
    /// breakpoint import. A guest `brk` immediate is 16 bits unsigned, so a
    /// negative value cannot be one and the host tells them apart. See
    /// `tier1::emit_sp_check`.
    pub const BRK_BAD_SP: i32 = -1;

    /// Not a `brk`: a diagnostic breakpoint planted at a chosen guest pc (see
    /// `tier1::emit_trap_pc`). The host dumps the live registers and RETURNS, so
    /// the block runs on -- unlike `BRK_BAD_SP`, which exits.
    pub const BRK_TRAP_PC: i32 = -2;

    /// Not a `brk`: a block-entry trace point (`GEX_TRACE_BLOCKS`), planted at
    /// the start of every block so a run can be followed block by block. The
    /// host prints the pc and a few registers and RETURNS.
    pub const BRK_TRACE: i32 = -3;

    /// Not a `brk`: a `Stmt::Trap` for an integer division by zero (x86
    /// `div`/`idiv`, #DE). The host raises it and does not return: SIGFPE
    /// with `FPE_INTDIV` for a Linux guest, EXCEPTION_INT_DIVIDE_BY_ZERO for
    /// a Windows one.
    pub const BRK_DIVIDE_BY_ZERO: i32 = -4;

    /// Not a `brk`: a `Stmt::Trap` for an integer division whose quotient
    /// does not fit (x86 #DE: `INT_MIN / -1`, or any quotient too wide for
    /// the destination). Linux reports it as SIGFPE with `FPE_INTDIV`, the
    /// same as a zero divisor; Windows as EXCEPTION_INT_OVERFLOW.
    pub const BRK_DIVIDE_OVERFLOW: i32 = -5;

    /// Not a `brk`: a `Stmt::Trap` for a general-protection fault, such as
    /// a privileged instruction (x86 `hlt`) in user mode. Linux reports it
    /// as SIGSEGV at the instruction.
    pub const BRK_GENERAL_PROTECTION: i32 = -6;

    /// Not a `brk`: a `Stmt::Trap` for an x87 exception the control word
    /// leaves unmasked. gex does not deliver x87 exceptions (a real x87
    /// would fault at the next x87 instruction), so the host fails loudly.
    pub const BRK_X87_EXCEPTION: i32 = -7;

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
    /// operands and result as bit patterns (single precision in the low 32
    /// bits). Wasm has no FMA and a separate multiply-then-add rounds twice,
    /// enough to break musl's `pow`, so this asks the host rather than
    /// approximating.
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
    /// Writes one of the module's passive element segments into the imported
    /// tables, chosen by index. See `Module::add_fill`: passive segments let a
    /// module's blocks reach a table without every process paying for every
    /// block at instantiation.
    pub const FILL: &str = "gexfill";
    /// Writes whichever passive segment holds a given dispatch slot, which is
    /// the question a miss can actually ask. See `Module::add_fill_at`.
    pub const FILL_AT: &str = "gexfillat";
    /// Writes every passive segment in one call, which is what an eager fill
    /// wants: a crossing per segment costs more than the writes.
    pub const FILL_ALL: &str = "gexfillall";

    /// The dispatch tables, several because V8 caps one at ten million
    /// entries; see [`super::DISPATCH_TABLES`]. Table 0 keeps the bare name
    /// `dispatch` for compatibility and holds the miss span an unwritten
    /// directory entry of zero points at.
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
    /// Not a cache operation: a block was entered for a neighbouring address
    /// that shares its dispatch slot (x86-64, see `DISPATCH_SHIFT`). The host
    /// forgets that address so the miss that follows re-translates it and the
    /// slot gets a trampoline covering both.
    pub const SLOT_CONFLICT: i32 = 9;
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

/// Table slot for a guest address, given its page's directory entry. Masks off
/// the table index, so a caller holding only the base (table 0) can pass it.
#[inline]
pub const fn dispatch_index(pc: u64, page_entry: u32) -> u64 {
    dispatch_entry_base(page_entry) as u64 + ((pc % DISPATCH_PAGE) >> DISPATCH_SHIFT)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The context layout is a compile-time contract between codegen, the
    // kernel and the host, so it is checked at compile time.
    const _: () = {
        assert!(ctx::x(0) == 0);
        assert!(ctx::x(30) == 240);
        assert!(ctx::PC > ctx::x(30));
        assert!(ctx::NF > ctx::PC);
        assert!(ctx::TPIDR > ctx::VF);
        assert!(ctx::V >= ctx::LAST_SYSCALL + 8);
        assert!(ctx::X87 == ctx::v(31) + 16);
        assert!(ctx::X87 % 16 == 0 && ctx::X87_TMP % 16 == 0);
        assert!(!X86 || ctx::SIZE >= ctx::SCRATCH + 64);
        assert!(X86 || PPC || ctx::SIZE == ctx::v(31) + 16);
        // PowerPC's registers fit between `v31` and the scratch area.
        assert!(ctx::PPC_ORIG_R3 + 8 <= ctx::SCRATCH);
        assert!(ctx::SIZE % 8 == 0);
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
        // Every address code may occupy needs a directory entry, including the
        // mmap arena where `ld.so` maps and calls into libraries.
        assert!(DISPATCH_MAX_PAGES * DISPATCH_PAGE >= STACK_TOP);
        assert!(DISPATCH_MAX_PAGES * DISPATCH_PAGE >= MMAP_TOP);
        assert!(EXEC_BASE < PIE_BASE);
        assert!(MMAP_BOTTOM < MMAP_TOP);
        // The stack sits at the top of the fixed low region, and the arena
        // starts above it and grows upward.
        assert!(PIE_BASE < INTERP_BASE);
        assert!(INTERP_BASE + 0x0010_0000 <= STACK_TOP - STACK_SIZE);
        assert!(INTERP_BASE < STACK_TOP);
        assert!(STACK_TOP <= MMAP_BOTTOM);
        assert!(MMAP_TOP <= GUEST_LIMIT);
    }
}
