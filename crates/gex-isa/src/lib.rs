//! `gex-isa` — AArch64 A64 instruction decoder.
//!
//! The decoder turns a 32-bit little-endian A64 encoding into a structured
//! [`Inst`]. Design rules that the rest of the compiler depends on:
//!
//! * **Total.** Every one of the 2^32 encodings maps to *some* `Inst`; anything
//!   not understood becomes [`Inst::Unknown`] (translate-time error / SIGILL)
//!   or [`Inst::Undefined`] (architecturally UNDEFINED → SIGILL). The decoder
//!   never panics and never silently misdecodes into a wrong opcode.
//! * **Register roles resolved at decode time.** Encoding bit patterns say
//!   `Rn = 31`; whether that means the zero register or the stack pointer is a
//!   per-encoding rule. Resolving it here (into [`RegSel`]) keeps that rule out
//!   of the lifter, where getting it wrong is a silent miscompile.
//! * **Immediates fully decoded.** Bitmask immediates, scaled load offsets and
//!   branch displacements are decoded to final values, not raw fields.

#![forbid(unsafe_code)]

pub mod decode;
pub mod fmt;

pub use decode::decode;

/// A general-purpose register operand, with the meaning of encoding value 31
/// already resolved.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum RegSel {
    /// `x0`–`x30` / `w0`–`w30`.
    Gp(u8),
    /// The zero register: reads as 0, writes discarded.
    Zr,
    /// The stack pointer.
    Sp,
}

impl RegSel {
    /// Decode a register field where 31 means the zero register.
    #[inline]
    pub fn gp(n: u32) -> RegSel {
        let n = (n & 31) as u8;
        if n == 31 {
            RegSel::Zr
        } else {
            RegSel::Gp(n)
        }
    }

    /// Decode a register field where 31 means the stack pointer.
    #[inline]
    pub fn sp(n: u32) -> RegSel {
        let n = (n & 31) as u8;
        if n == 31 {
            RegSel::Sp
        } else {
            RegSel::Gp(n)
        }
    }
}

/// A SIMD&FP register `v0`–`v31` (also `b/h/s/d/q` views of the same file).
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct VReg(pub u8);

/// Operand width for integer data processing.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Sz {
    /// 32-bit (`w`) — results are zero-extended into the 64-bit register.
    W,
    /// 64-bit (`x`).
    X,
}

impl Sz {
    #[inline]
    pub fn from_sf(sf: u32) -> Sz {
        if sf & 1 != 0 {
            Sz::X
        } else {
            Sz::W
        }
    }
    #[inline]
    pub fn bits(self) -> u32 {
        match self {
            Sz::W => 32,
            Sz::X => 64,
        }
    }
    #[inline]
    pub fn is64(self) -> bool {
        matches!(self, Sz::X)
    }
    /// Mask of valid bits for this width.
    #[inline]
    pub fn mask(self) -> u64 {
        match self {
            Sz::W => 0xffff_ffff,
            Sz::X => u64::MAX,
        }
    }
}

/// Condition code (`b.<cond>`, `csel`, `ccmp`, …).
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Cond(pub u8);

impl Cond {
    pub const EQ: Cond = Cond(0x0);
    pub const NE: Cond = Cond(0x1);
    pub const AL: Cond = Cond(0xe);

    /// The condition with its low bit flipped — i.e. the logical inverse,
    /// except that `AL`/`NV` (0b111x) both mean "always" and are not inverses.
    #[inline]
    pub fn invert(self) -> Cond {
        Cond(self.0 ^ 1)
    }
    /// True for `AL` and `NV`, which the architecture defines as always-true.
    #[inline]
    pub fn is_always(self) -> bool {
        self.0 >= 0b1110
    }
    pub fn name(self) -> &'static str {
        const N: [&str; 16] = [
            "eq", "ne", "cs", "cc", "mi", "pl", "vs", "vc", "hi", "ls", "ge", "lt", "gt", "le",
            "al", "nv",
        ];
        N[(self.0 & 15) as usize]
    }
}

/// Shift applied to the second source of a data-processing-register op.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum ShiftType {
    Lsl,
    Lsr,
    Asr,
    Ror,
}

impl ShiftType {
    #[inline]
    pub fn from_bits(b: u32) -> ShiftType {
        match b & 3 {
            0 => ShiftType::Lsl,
            1 => ShiftType::Lsr,
            2 => ShiftType::Asr,
            _ => ShiftType::Ror,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            ShiftType::Lsl => "lsl",
            ShiftType::Lsr => "lsr",
            ShiftType::Asr => "asr",
            ShiftType::Ror => "ror",
        }
    }
}

/// Register extension applied by `add/sub (extended)` and register-offset
/// addressing modes.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Extend {
    Uxtb,
    Uxth,
    Uxtw,
    Uxtx,
    Sxtb,
    Sxth,
    Sxtw,
    Sxtx,
}

impl Extend {
    #[inline]
    pub fn from_bits(b: u32) -> Extend {
        match b & 7 {
            0 => Extend::Uxtb,
            1 => Extend::Uxth,
            2 => Extend::Uxtw,
            3 => Extend::Uxtx,
            4 => Extend::Sxtb,
            5 => Extend::Sxth,
            6 => Extend::Sxtw,
            _ => Extend::Sxtx,
        }
    }
    /// Number of source bits consumed before extension.
    #[inline]
    pub fn src_bits(self) -> u32 {
        match self {
            Extend::Uxtb | Extend::Sxtb => 8,
            Extend::Uxth | Extend::Sxth => 16,
            Extend::Uxtw | Extend::Sxtw => 32,
            Extend::Uxtx | Extend::Sxtx => 64,
        }
    }
    #[inline]
    pub fn is_signed(self) -> bool {
        matches!(self, Extend::Sxtb | Extend::Sxth | Extend::Sxtw | Extend::Sxtx)
    }
    pub fn name(self) -> &'static str {
        match self {
            Extend::Uxtb => "uxtb",
            Extend::Uxth => "uxth",
            Extend::Uxtw => "uxtw",
            Extend::Uxtx => "uxtx",
            Extend::Sxtb => "sxtb",
            Extend::Sxth => "sxth",
            Extend::Sxtw => "sxtw",
            Extend::Sxtx => "sxtx",
        }
    }
}

/// Add or subtract.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum AddOp {
    Add,
    Sub,
}

/// Bitwise operation family.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum LogOp {
    And,
    Orr,
    Eor,
}

/// `movz` / `movn` / `movk`.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum MoveOp {
    /// Zero the register, then insert the immediate.
    Movz,
    /// Insert the immediate, then invert every bit.
    Movn,
    /// Insert the immediate, keeping the other bits.
    Movk,
}

/// Bitfield move variant.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum BfOp {
    /// Signed: replicate the top copied bit leftwards.
    Sbfm,
    /// Unsigned: zero outside the copied field.
    Ubfm,
    /// Leave bits outside the field untouched.
    Bfm,
}

/// One-source integer data processing.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Dp1Op {
    Rbit,
    Rev16,
    Rev32,
    Rev,
    Clz,
    Cls,
}

/// Two-source integer data processing.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Dp2Op {
    Udiv,
    Sdiv,
    Lslv,
    Lsrv,
    Asrv,
    Rorv,
    Crc32b,
    Crc32h,
    Crc32w,
    Crc32x,
    Crc32cb,
    Crc32ch,
    Crc32cw,
    Crc32cx,
}

/// Three-source integer data processing (multiply-accumulate family).
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Dp3Op {
    /// `rd = ra + rn * rm`
    Madd,
    /// `rd = ra - rn * rm`
    Msub,
    /// `rd = ra + sext(wn) * sext(wm)`
    Smaddl,
    Smsubl,
    Umaddl,
    Umsubl,
    /// High 64 bits of the signed 64x64 product.
    Smulh,
    Umulh,
}

/// Memory access size in bytes.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum MemSz {
    B = 1,
    H = 2,
    W = 4,
    D = 8,
    Q = 16,
}

impl MemSz {
    #[inline]
    pub fn bytes(self) -> u32 {
        self as u32
    }
    #[inline]
    pub fn log2(self) -> u32 {
        match self {
            MemSz::B => 0,
            MemSz::H => 1,
            MemSz::W => 2,
            MemSz::D => 3,
            MemSz::Q => 4,
        }
    }
    #[inline]
    pub fn from_log2(l: u32) -> MemSz {
        match l & 7 {
            0 => MemSz::B,
            1 => MemSz::H,
            2 => MemSz::W,
            3 => MemSz::D,
            _ => MemSz::Q,
        }
    }
}

/// How a load/store computes its address and whether it writes the base back.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum AddrMode {
    /// `[rn, #off]` — offset already scaled by the access size.
    Offset { rn: RegSel, off: i64 },
    /// `[rn], #off` — access at `rn`, then `rn += off`.
    PostIndex { rn: RegSel, off: i64 },
    /// `[rn, #off]!` — `rn += off`, then access at `rn`.
    PreIndex { rn: RegSel, off: i64 },
    /// `[rn, rm, <ext> #amount]`.
    RegOffset {
        rn: RegSel,
        rm: RegSel,
        ext: Extend,
        amount: u32,
    },
    /// PC-relative literal: the final absolute guest address.
    Literal { addr: u64 },
}

/// Memory ordering requested by a load/store.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Ordering {
    /// Plain access, no ordering.
    None,
    /// Load-acquire.
    Acquire,
    /// Store-release.
    Release,
}

/// Barrier flavour.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum BarrierKind {
    /// Data memory barrier.
    Dmb,
    /// Data synchronization barrier.
    Dsb,
    /// Instruction synchronization barrier — also our translation-invalidation
    /// hook for self-modifying guest code (PLAN §6.2).
    Isb,
    /// Speculation barrier.
    Sb,
}

/// Atomic read-modify-write operation (FEAT_LSE).
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum RmwOp {
    Add,
    Clr,
    Eor,
    Set,
    Smax,
    Smin,
    Umax,
    Umin,
    /// Plain swap.
    Swp,
}

/// A decoded A64 instruction.
///
/// Variants carry fully-resolved operands: the lifter should never need to
/// re-examine raw encoding bits.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Inst {
    /// Not recognised by this decoder — a translator gap, not necessarily an
    /// invalid encoding. Translating one is a hard error; executing one at
    /// runtime raises SIGILL.
    Unknown { raw: u32 },
    /// Architecturally UNDEFINED, or `udf`. Executing raises SIGILL.
    Undefined { raw: u32 },

    /// `nop`, `yield`, `wfe`, … — architecturally a hint, semantically a no-op
    /// for us.
    Hint { imm: u32 },

    /// `adr rd, label` — `target` is the resolved absolute guest address.
    Adr { rd: RegSel, target: u64 },
    /// `adrp rd, label` — `target` is the resolved absolute *page* address
    /// (`(pc & !0xfff) + imm`), already 4 KiB aligned.
    Adrp { rd: RegSel, target: u64 },

    /// `add`/`sub`/`adds`/`subs` (immediate). `rd`/`rn` are SP-form unless
    /// flags are set.
    AddSubImm {
        sz: Sz,
        op: AddOp,
        set_flags: bool,
        rd: RegSel,
        rn: RegSel,
        imm: u64,
    },
    /// `and`/`orr`/`eor`/`ands` (bitmask immediate), immediate already expanded.
    LogicalImm {
        sz: Sz,
        op: LogOp,
        set_flags: bool,
        rd: RegSel,
        rn: RegSel,
        imm: u64,
    },
    /// `movz`/`movn`/`movk`.
    MoveWide {
        sz: Sz,
        op: MoveOp,
        rd: RegSel,
        imm16: u64,
        shift: u32,
    },
    /// `sbfm`/`ubfm`/`bfm` — covers `lsl`/`lsr`/`asr`/`ubfx`/`sbfx`/`uxtb`/…
    Bitfield {
        sz: Sz,
        op: BfOp,
        rd: RegSel,
        rn: RegSel,
        immr: u32,
        imms: u32,
    },
    /// `extr rd, rn, rm, #lsb` (and its `ror` alias).
    Extract {
        sz: Sz,
        rd: RegSel,
        rn: RegSel,
        rm: RegSel,
        lsb: u32,
    },

    /// `add`/`sub`/`adds`/`subs` (shifted register).
    AddSubShifted {
        sz: Sz,
        op: AddOp,
        set_flags: bool,
        rd: RegSel,
        rn: RegSel,
        rm: RegSel,
        shift: ShiftType,
        amount: u32,
    },
    /// `add`/`sub`/`adds`/`subs` (extended register). `rn` is SP-form.
    AddSubExtended {
        sz: Sz,
        op: AddOp,
        set_flags: bool,
        rd: RegSel,
        rn: RegSel,
        rm: RegSel,
        ext: Extend,
        amount: u32,
    },
    /// `adc`/`sbc`/`adcs`/`sbcs`.
    AddSubCarry {
        sz: Sz,
        op: AddOp,
        set_flags: bool,
        rd: RegSel,
        rn: RegSel,
        rm: RegSel,
    },
    /// `and`/`bic`/`orr`/`orn`/`eor`/`eon`/`ands`/`bics` (shifted register).
    LogicalShifted {
        sz: Sz,
        op: LogOp,
        set_flags: bool,
        /// `bic`/`orn`/`eon`: invert the shifted second operand.
        invert: bool,
        rd: RegSel,
        rn: RegSel,
        rm: RegSel,
        shift: ShiftType,
        amount: u32,
    },
    /// `csel`/`csinc`/`csinv`/`csneg` — `rd = cond ? rn : f(rm)`.
    CondSelect {
        sz: Sz,
        rd: RegSel,
        rn: RegSel,
        rm: RegSel,
        cond: Cond,
        /// Add 1 to the else-value (`csinc`, `cset`, `cinc`).
        else_inc: bool,
        /// Bitwise-invert the else-value (`csinv`/`csneg`, `csetm`).
        else_inv: bool,
    },
    /// `ccmp`/`ccmn` (register or immediate second operand).
    CondCompare {
        sz: Sz,
        /// `ccmp` subtracts; `ccmn` adds.
        op: AddOp,
        rn: RegSel,
        /// Register operand, or `None` when `imm` is used.
        rm: Option<RegSel>,
        imm: u64,
        /// Flags to install when the condition is false.
        nzcv: u8,
        cond: Cond,
    },
    /// One-source data processing.
    DataProc1 {
        sz: Sz,
        op: Dp1Op,
        rd: RegSel,
        rn: RegSel,
    },
    /// Two-source data processing.
    DataProc2 {
        sz: Sz,
        op: Dp2Op,
        rd: RegSel,
        rn: RegSel,
        rm: RegSel,
    },
    /// Three-source data processing.
    DataProc3 {
        sz: Sz,
        op: Dp3Op,
        rd: RegSel,
        rn: RegSel,
        rm: RegSel,
        ra: RegSel,
    },

    /// `b` / `bl` to an absolute guest address.
    Branch { target: u64, link: bool },
    /// `b.<cond>` to an absolute guest address.
    BranchCond { cond: Cond, target: u64 },
    /// `cbz` / `cbnz`.
    CompareBranch {
        sz: Sz,
        rt: RegSel,
        target: u64,
        /// `cbnz` when true.
        nonzero: bool,
    },
    /// `tbz` / `tbnz`.
    TestBranch {
        rt: RegSel,
        bit: u32,
        target: u64,
        /// `tbnz` when true.
        nonzero: bool,
    },
    /// `br` / `blr` / `ret` — indirect branch through a register.
    BranchReg {
        rn: RegSel,
        link: bool,
        /// `ret`: hint that this pairs with an earlier `bl`.
        is_ret: bool,
    },

    /// `svc #imm` — the syscall entry point.
    Svc { imm: u32 },
    /// `brk #imm` — debug breakpoint, raises SIGTRAP.
    Brk { imm: u32 },

    /// `mrs rt, <sysreg>` — read a system register.
    SysRegRead { rt: RegSel, sysreg: SysReg },
    /// `msr <sysreg>, rt` — write a system register.
    SysRegWrite { rt: RegSel, sysreg: SysReg },
    /// `dc`/`ic`/`at`/`tlbi` — cache and TLB maintenance.
    SysMaintenance { op: SysMaintOp, rt: RegSel },
    /// `dmb`/`dsb`/`isb`/`sb`.
    Barrier { kind: BarrierKind, option: u32 },
    /// `clrex` — clear the local exclusive monitor.
    Clrex,

    /// A plain (non-atomic) load or store of an integer register.
    Load {
        sz: MemSz,
        /// Destination register width; `LDRSW` loads 4 bytes into an X.
        dest: Sz,
        /// Sign-extend the loaded value to `dest`.
        signed: bool,
        rt: RegSel,
        addr: AddrMode,
        order: Ordering,
    },
    Store {
        sz: MemSz,
        rt: RegSel,
        addr: AddrMode,
        order: Ordering,
    },
    /// `ldp`/`ldpsw`.
    LoadPair {
        sz: MemSz,
        signed: bool,
        rt: RegSel,
        rt2: RegSel,
        addr: AddrMode,
    },
    /// `stp`.
    StorePair {
        sz: MemSz,
        rt: RegSel,
        rt2: RegSel,
        addr: AddrMode,
    },
    /// `prfm` — prefetch; a no-op for us but must not decode as Unknown.
    Prefetch { addr: AddrMode },

    /// `ldxr`/`ldaxr` — load-exclusive, arms the local monitor.
    LoadExclusive {
        sz: MemSz,
        rt: RegSel,
        rn: RegSel,
        order: Ordering,
    },
    /// `stxr`/`stlxr` — store-exclusive; `rs` receives 0 on success.
    StoreExclusive {
        sz: MemSz,
        rs: RegSel,
        rt: RegSel,
        rn: RegSel,
        order: Ordering,
    },
    /// `cas`/`casa`/`casl`/`casal` — compare and swap (FEAT_LSE).
    CompareSwap {
        sz: MemSz,
        rs: RegSel,
        rt: RegSel,
        rn: RegSel,
        acquire: bool,
        release: bool,
    },
    /// `ldadd`/`ldset`/`swp`/… — atomic read-modify-write (FEAT_LSE).
    AtomicRmw {
        sz: MemSz,
        op: RmwOp,
        rs: RegSel,
        rt: RegSel,
        rn: RegSel,
        acquire: bool,
        release: bool,
    },
}

/// System registers we model. Anything else decodes to [`SysReg::Other`] and
/// traps to the runtime.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum SysReg {
    /// Thread pointer — the guest's TLS base.
    TpidrEl0,
    /// Read-only thread pointer.
    TpidrroEl0,
    /// Virtual counter, served from the shared time page.
    Cntvct,
    /// Counter frequency.
    Cntfrq,
    /// Cache type register.
    Ctr,
    /// Data cache zero ID.
    Dczid,
    /// Main ID register.
    Midr,
    /// FP control register.
    Fpcr,
    /// FP status register.
    Fpsr,
    /// NZCV as a system register.
    Nzcv,
    /// Encoded `op0:op1:crn:crm:op2`, unmodelled.
    Other(u16),
}

/// Cache/TLB maintenance operation.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum SysMaintOp {
    /// `dc zva` — zero a cache line; the guest memset fast path.
    DcZva,
    /// `dc cvau`/`civac`/… — data cache clean, a no-op for a coherent host.
    DcClean,
    /// `ic ivau`/`iallu` — instruction cache invalidate. Our translation
    /// invalidation hook (PLAN §6.2).
    IcInvalidate,
    /// Everything else (`at`, `tlbi`, …) — no-op.
    Other,
}

impl Inst {
    /// True when this instruction ends a basic block.
    pub fn is_terminator(&self) -> bool {
        matches!(
            self,
            Inst::Branch { .. }
                | Inst::BranchCond { .. }
                | Inst::CompareBranch { .. }
                | Inst::TestBranch { .. }
                | Inst::BranchReg { .. }
                | Inst::Svc { .. }
                | Inst::Brk { .. }
                | Inst::Unknown { .. }
                | Inst::Undefined { .. }
        )
    }

    /// For a direct branch, the statically known target.
    pub fn direct_target(&self) -> Option<u64> {
        match *self {
            Inst::Branch { target, .. }
            | Inst::BranchCond { target, .. }
            | Inst::CompareBranch { target, .. }
            | Inst::TestBranch { target, .. } => Some(target),
            _ => None,
        }
    }

    /// True when control may continue to the next instruction.
    pub fn falls_through(&self) -> bool {
        match self {
            Inst::Branch { link, .. } => *link,
            Inst::BranchReg { link, .. } => *link,
            Inst::BranchCond { .. } | Inst::CompareBranch { .. } | Inst::TestBranch { .. } => true,
            Inst::Svc { .. } | Inst::Brk { .. } => true,
            Inst::Unknown { .. } | Inst::Undefined { .. } => false,
            _ => true,
        }
    }
}
