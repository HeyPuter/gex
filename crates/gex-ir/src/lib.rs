//! GIR — gex's intermediate representation for binary translation.
//!
//! GIR is deliberately small and *not* a general-purpose compiler IR (PLAN
//! §15). It models exactly what a machine-code translator needs:
//!
//! * guest register reads and writes as first-class nodes, so register
//!   caching and dead-store elimination are ordinary IR passes;
//! * condition flags as four independent one-bit registers, because that is
//!   how AArch64 consumes them and it makes dead-flag elimination trivial;
//! * memory operations with explicit ordering, kept in program order;
//! * terminators that name *guest* addresses, so a block never needs to know
//!   how the backend will reach its successors.
//!
//! A [`Block`] is a straight-line list of [`Stmt`]s ending in one [`Term`].
//! Values are SSA: each [`Value`] is defined exactly once, by the `Stmt::Def`
//! that introduces it.

#![forbid(unsafe_code)]

pub mod pass;
pub mod print;

use std::fmt;

/// A GIR value.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Value(pub u32);

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}

/// GIR is typed with exactly the two integer types wasm gives us. Guest
/// 8- and 16-bit values live zero- or sign-extended inside an `I32`.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Ty {
    I32,
    I64,
    /// A 128-bit vector — one AArch64 `v` register, one wasm `v128`.
    V128,
}

impl Ty {
    /// Width in bits.
    pub fn bits(self) -> u32 {
        match self {
            Ty::I32 => 32,
            Ty::I64 => 64,
            Ty::V128 => 128,
        }
    }

    /// All-ones mask for this width, as a `u64`. Only meaningful for the
    /// scalar types; vectors are never handled as a single integer.
    pub fn mask(self) -> u64 {
        match self {
            Ty::I32 => 0xffff_ffff,
            Ty::I64 | Ty::V128 => u64::MAX,
        }
    }
}

/// A piece of architectural guest state addressable as a register.
///
/// Everything here lives in the thread context struct; the backend decides
/// whether a given access hits memory or a cached wasm local.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum GuestReg {
    /// `x0`–`x30`.
    X(u8),
    /// `v0`–`v31`, the SIMD and floating-point register file.
    V128(u8),
    /// Stack pointer.
    Sp,
    /// Program counter. Only written at block boundaries and safepoints.
    Pc,
    /// Negative flag.
    N,
    /// Zero flag.
    Z,
    /// Carry flag.
    C,
    /// Overflow flag.
    V,
    /// `TPIDR_EL0`, the guest thread pointer.
    Tpidr,
    /// `TPIDRRO_EL0`.
    TpidrRo,
    /// Exclusive monitor: tagged address.
    MonAddr,
    /// Exclusive monitor: value observed by the load-exclusive.
    MonValue,
    /// Exclusive monitor: access size in bytes, 0 when clear.
    MonSize,
}

impl GuestReg {
    /// Width of this register's storage.
    pub fn ty(self) -> Ty {
        match self {
            GuestReg::N | GuestReg::Z | GuestReg::C | GuestReg::V => Ty::I32,
            GuestReg::V128(_) => Ty::V128,
            _ => Ty::I64,
        }
    }

    /// True for the four condition flags.
    pub fn is_flag(self) -> bool {
        matches!(self, GuestReg::N | GuestReg::Z | GuestReg::C | GuestReg::V)
    }
}

/// Binary operations. Both operands and the result share one [`Ty`].
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    And,
    Or,
    Xor,
    /// Shift left; the amount is masked to the operand width, which is what
    /// both wasm and AArch64's variable-shift instructions do.
    Shl,
    /// Logical shift right.
    ShrU,
    /// Arithmetic shift right.
    ShrS,
    /// Rotate right.
    Rotr,
    /// Unsigned division. AArch64 defines division by zero as producing zero
    /// while wasm traps, so the lifter guards the divisor.
    DivU,
    /// Signed division. Also needs guarding for `INT_MIN / -1`.
    DivS,
    RemU,
    RemS,
}

/// Unary operations.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum UnOp {
    /// Count leading zeros.
    Clz,
    /// Count trailing zeros.
    Ctz,
    /// Population count.
    Popcnt,
    /// Bitwise complement.
    Not,
    /// Two's-complement negation.
    Neg,
}

/// Integer comparisons. The result is an `I32` that is 0 or 1.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum CmpOp {
    Eq,
    Ne,
    LtU,
    LtS,
    LeU,
    LeS,
    GtU,
    GtS,
    GeU,
    GeS,
}

/// Memory access width in bytes.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum MemSz {
    B1,
    B2,
    B4,
    B8,
}

impl MemSz {
    pub fn bytes(self) -> u32 {
        match self {
            MemSz::B1 => 1,
            MemSz::B2 => 2,
            MemSz::B4 => 4,
            MemSz::B8 => 8,
        }
    }
}

/// Memory ordering attached to a load, store or read-modify-write.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Order {
    /// Plain access. Guest data races stay data races (PLAN §5.8).
    Relaxed,
    /// Sequentially consistent — used for every *ordered* guest access, since
    /// seq-cst is stronger than AArch64 acquire/release and is therefore
    /// always a correct over-approximation.
    SeqCst,
}

/// Atomic read-modify-write operations that map to single wasm instructions.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum RmwOp {
    Add,
    Sub,
    And,
    Or,
    Xor,
    Xchg,
}

/// A value-producing expression.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Expr {
    ConstI32(u32),
    ConstI64(u64),
    /// Read architectural state.
    ReadReg(GuestReg),
    Bin {
        op: BinOp,
        ty: Ty,
        a: Value,
        b: Value,
    },
    Un {
        op: UnOp,
        ty: Ty,
        a: Value,
    },
    /// Compare two `ty`-typed operands, producing `I32` 0 or 1.
    Cmp {
        op: CmpOp,
        ty: Ty,
        a: Value,
        b: Value,
    },
    /// `cond ? a : b`, where `cond` is an `I32` boolean.
    Select {
        cond: Value,
        a: Value,
        b: Value,
        ty: Ty,
    },
    /// Narrow an `I64` to an `I32`.
    Wrap(Value),
    /// Widen an `I32` to an `I64`.
    Extend {
        a: Value,
        signed: bool,
    },
    /// Sign- or zero-extend the low `bits` of a `ty`-typed value, result also
    /// `ty`-typed.
    ExtendBits {
        a: Value,
        ty: Ty,
        bits: u32,
        signed: bool,
    },
    /// Read guest memory at `addr + offset`, where `addr` is a full 64-bit
    /// guest address.
    Load {
        addr: Value,
        offset: i64,
        sz: MemSz,
        signed: bool,
        ty: Ty,
        order: Order,
    },
    /// Atomic read-modify-write; produces the value read.
    Rmw {
        op: RmwOp,
        addr: Value,
        val: Value,
        sz: MemSz,
        ty: Ty,
    },
    /// Compare-and-swap; produces the value read.
    CmpXchg {
        addr: Value,
        expected: Value,
        replacement: Value,
        sz: MemSz,
        ty: Ty,
    },
    /// Call a host import that returns a value.
    HostCall {
        func: HostFunc,
        args: Vec<Value>,
        ty: Ty,
    },

    /// Load `bytes` bytes into the low lanes of a vector, zeroing the rest.
    /// This is how every SIMD&FP load works: writing `s0` clears bits 32-127.
    VLoad {
        addr: Value,
        offset: i64,
        bytes: u32,
    },
    /// A vector literal.
    VConst(Box<[u8; 16]>),
    /// Replicate a scalar into every `lane_bytes`-sized lane.
    VSplat { a: Value, lane_bytes: u32 },
    /// Bitwise operation on two vectors.
    VBin { op: VBinOp, a: Value, b: Value },
    /// Lane-wise unary operation on a vector.
    VUn { op: VUnOp, a: Value },
    /// Lane-wise binary operation.
    VLaneBin {
        op: VLaneBinOp,
        lane: VLane,
        a: Value,
        b: Value,
    },
    /// Lane-wise unary operation.
    VLaneUn {
        op: VLaneUnOp,
        lane: VLane,
        a: Value,
    },
    /// Lane-wise shift by an `I32` amount shared across lanes.
    VShift {
        op: VShiftOp,
        lane: VLane,
        a: Value,
        amount: Value,
    },
    /// Byte-level permutation of two vectors.
    ///
    /// This single node covers `trn`, `uzp`, `zip`, `ext`, the `rev` family
    /// and element `dup`/`ins`, because every one of them is a fixed byte
    /// permutation and wasm has exactly that instruction.
    VShuffle {
        a: Value,
        b: Value,
        lanes: Box<[u8; 16]>,
    },
    /// Byte-level permutation by a runtime index vector; out-of-range indices
    /// produce zero, which is what `tbl` with a single table register does.
    VSwizzle { a: Value, idx: Value },
    /// `(a & mask) | (b & !mask)`.
    VBitSelect {
        mask: Value,
        a: Value,
        b: Value,
    },
    /// Lane conversion.
    VCvt {
        op: VCvtOp,
        /// Lane type of the source.
        from: VLane,
        a: Value,
        /// Second operand, only for the narrowing conversions.
        b: Option<Value>,
    },
    /// Read one lane out as a scalar.
    VExtract {
        a: Value,
        lane: u32,
        lane_bytes: u32,
        signed: bool,
        /// Type of the extracted scalar.
        ty: Ty,
    },
    /// Write one lane, producing a new vector.
    VReplace {
        a: Value,
        lane: u32,
        lane_bytes: u32,
        val: Value,
    },

    /// Scalar floating-point arithmetic on bit patterns.
    FpBin {
        op: FpBinOp,
        /// True for double precision, false for single.
        double: bool,
        a: Value,
        b: Value,
    },
    FpUn {
        op: FpUnOp,
        double: bool,
        a: Value,
    },
    /// Compare two floats, producing an `I32` boolean. Any comparison
    /// involving a NaN is false, which is IEEE-754's rule and wasm's.
    FpCmp {
        op: FpCmpOp,
        double: bool,
        a: Value,
        b: Value,
    },
    /// Convert between floating-point and integer representations.
    FpCvt {
        kind: FpConvert,
        /// Width of the source: true for 64-bit.
        src_wide: bool,
        /// Width of the destination: true for 64-bit.
        dst_wide: bool,
        a: Value,
    },
}

/// Scalar floating-point operations.
///
/// FP values travel through GIR as their integer *bit patterns* (`I32` for
/// single, `I64` for double), and the backend reinterprets around each
/// operation. That keeps the type lattice at three entries and matches how
/// AArch64 itself stores them — a `d` register is just the low half of a `v`
/// register.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum FpBinOp {
    Add,
    Sub,
    Mul,
    Div,
    /// IEEE-754 minimum, propagating NaN — AArch64's `FMIN`.
    Min,
    Max,
    Copysign,
}

/// Scalar floating-point unary operations.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum FpUnOp {
    Abs,
    Neg,
    Sqrt,
    /// Round to nearest, ties to even.
    RoundNearest,
    /// Round toward +infinity.
    Ceil,
    /// Round toward -infinity.
    Floor,
    /// Round toward zero.
    Trunc,
}

/// Scalar floating-point comparisons, producing an `I32` boolean.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum FpCmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A conversion between a floating-point value and an integer, or between
/// the two floating-point widths.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum FpConvert {
    /// Integer to float. `signed` selects the interpretation of the source.
    IntToFp { signed: bool },
    /// Float to integer, rounding toward zero and *saturating* — which is
    /// what AArch64's `FCVTZ*` does, and why the lifter uses wasm's
    /// non-trapping conversions rather than the trapping ones.
    FpToInt { signed: bool },
    /// Single to double, or double to single.
    FpToFp,
}

/// The lane type a vector operation works on.
///
/// AArch64 spells this as an arrangement (`v0.4s`); GIR keeps only the lane
/// type, because the number of lanes follows from the 128-bit register and
/// the 64-bit forms are handled by masking the result.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum VLane {
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
}

impl VLane {
    pub fn bytes(self) -> u32 {
        match self {
            VLane::I8 => 1,
            VLane::I16 => 2,
            VLane::I32 | VLane::F32 => 4,
            VLane::I64 | VLane::F64 => 8,
        }
    }
    pub fn is_float(self) -> bool {
        matches!(self, VLane::F32 | VLane::F64)
    }
    /// The integer lane type of the same width.
    pub fn as_int(self) -> VLane {
        match self {
            VLane::F32 => VLane::I32,
            VLane::F64 => VLane::I64,
            other => other,
        }
    }
}

/// Lane-wise binary operations.
///
/// Comparisons write all-ones or all-zeros per lane, which is what both NEON
/// and wasm SIMD do, so they compose directly with `bitselect`.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum VLaneBinOp {
    Add,
    Sub,
    Mul,
    Div,
    MinS,
    MinU,
    MaxS,
    MaxU,
    AddSatS,
    AddSatU,
    SubSatS,
    SubSatU,
    AvgrU,
    Eq,
    Ne,
    LtS,
    LtU,
    GtS,
    GtU,
    LeS,
    LeU,
    GeS,
    GeU,
}

/// Lane-wise unary operations.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum VLaneUnOp {
    Neg,
    Abs,
    Sqrt,
    Ceil,
    Floor,
    Trunc,
    Nearest,
}

/// Lane-wise shift by a scalar amount shared across lanes.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum VShiftOp {
    Shl,
    ShrS,
    ShrU,
}

/// Lane-width and representation conversions.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum VCvtOp {
    /// Float lanes to integer lanes, rounding toward zero and saturating.
    FpToIntSat { signed: bool },
    /// Integer lanes to float lanes.
    IntToFp { signed: bool },
    /// `f64x2` to the low two lanes of an `f32x4`.
    DemoteF64,
    /// The low two lanes of an `f32x4` to `f64x2`.
    PromoteF32,
    /// Widen the low half of the source's lanes.
    ExtendLow { signed: bool },
    /// Widen the high half.
    ExtendHigh { signed: bool },
    /// Halve the lane width of two vectors into one, saturating.
    NarrowSat { signed: bool },
}

/// Vector unary operations.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum VUnOp {
    /// Per-byte population count.
    PopcntBytes,
}

/// Bitwise vector operations.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum VBinOp {
    And,
    Or,
    Xor,
    /// `a & !b`.
    AndNot,
}

impl Expr {
    /// True when the expression can be reordered or deleted freely.
    ///
    /// Loads are *not* pure: they must keep their position relative to stores
    /// and to other ordered accesses.
    pub fn is_pure(&self) -> bool {
        !matches!(
            self,
            Expr::Load { .. }
                | Expr::Rmw { .. }
                | Expr::CmpXchg { .. }
                | Expr::HostCall { .. }
                | Expr::VLoad { .. }
        )
    }

    /// The type of the value this expression produces.
    pub fn ty(&self) -> Ty {
        match *self {
            Expr::ConstI32(_) | Expr::Wrap(_) | Expr::Cmp { .. } => Ty::I32,
            Expr::ConstI64(_) | Expr::Extend { .. } => Ty::I64,
            Expr::ReadReg(r) => r.ty(),
            Expr::Bin { ty, .. }
            | Expr::Un { ty, .. }
            | Expr::Select { ty, .. }
            | Expr::ExtendBits { ty, .. }
            | Expr::Load { ty, .. }
            | Expr::Rmw { ty, .. }
            | Expr::CmpXchg { ty, .. }
            | Expr::HostCall { ty, .. }
            | Expr::VExtract { ty, .. } => ty,
            Expr::VLoad { .. }
            | Expr::VConst(_)
            | Expr::VSplat { .. }
            | Expr::VBin { .. }
            | Expr::VUn { .. }
            | Expr::VLaneBin { .. }
            | Expr::VLaneUn { .. }
            | Expr::VShift { .. }
            | Expr::VShuffle { .. }
            | Expr::VSwizzle { .. }
            | Expr::VBitSelect { .. }
            | Expr::VCvt { .. }
            | Expr::VReplace { .. } => Ty::V128,
            // Floating-point results are the bit pattern of the value, so
            // the width follows the precision.
            Expr::FpBin { double, .. } | Expr::FpUn { double, .. } => {
                if double {
                    Ty::I64
                } else {
                    Ty::I32
                }
            }
            Expr::FpCmp { .. } => Ty::I32,
            Expr::FpCvt { dst_wide, .. } => {
                if dst_wide {
                    Ty::I64
                } else {
                    Ty::I32
                }
            }
        }
    }

    /// Visit every value this expression reads.
    pub fn for_each_use(&self, mut f: impl FnMut(Value)) {
        match self {
            Expr::ConstI32(_) | Expr::ConstI64(_) | Expr::ReadReg(_) => {}
            Expr::Bin { a, b, .. } | Expr::Cmp { a, b, .. } => {
                f(*a);
                f(*b);
            }
            Expr::Un { a, .. } | Expr::Wrap(a) | Expr::Extend { a, .. } => f(*a),
            Expr::ExtendBits { a, .. } => f(*a),
            Expr::Select { cond, a, b, .. } => {
                f(*cond);
                f(*a);
                f(*b);
            }
            Expr::Load { addr, .. } => f(*addr),
            Expr::Rmw { addr, val, .. } => {
                f(*addr);
                f(*val);
            }
            Expr::CmpXchg {
                addr,
                expected,
                replacement,
                ..
            } => {
                f(*addr);
                f(*expected);
                f(*replacement);
            }
            Expr::HostCall { args, .. } => args.iter().copied().for_each(f),
            Expr::VLoad { addr, .. } => f(*addr),
            Expr::VConst(_) => {}
            Expr::VSplat { a, .. } | Expr::VExtract { a, .. } | Expr::VUn { a, .. } => f(*a),
            Expr::VLaneUn { a, .. } => f(*a),
            Expr::VLaneBin { a, b, .. } | Expr::VShuffle { a, b, .. } => {
                f(*a);
                f(*b);
            }
            Expr::VShift { a, amount, .. } => {
                f(*a);
                f(*amount);
            }
            Expr::VSwizzle { a, idx } => {
                f(*a);
                f(*idx);
            }
            Expr::VBitSelect { mask, a, b } => {
                f(*mask);
                f(*a);
                f(*b);
            }
            Expr::VCvt { a, b, .. } => {
                f(*a);
                if let Some(b) = b {
                    f(*b);
                }
            }
            Expr::VBin { a, b, .. } => {
                f(*a);
                f(*b);
            }
            Expr::VReplace { a, val, .. } => {
                f(*a);
                f(*val);
            }
            Expr::FpBin { a, b, .. } | Expr::FpCmp { a, b, .. } => {
                f(*a);
                f(*b);
            }
            Expr::FpUn { a, .. } | Expr::FpCvt { a, .. } => f(*a),
        }
    }
}

/// Host imports callable from translated code.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum HostFunc {
    /// `svc` — enter the kernel.
    Syscall,
    /// Report an undecodable or undefined instruction; raises SIGILL.
    Illegal,
    /// `brk`; raises SIGTRAP.
    Breakpoint,
    /// Read an unmodelled system register.
    SysRegRead,
    /// Write an unmodelled system register.
    SysRegWrite,
    /// Cache maintenance, including the translation-invalidation hook.
    CacheOp,
    /// Safepoint poll for signal delivery.
    Safepoint,
    /// Fused multiply-add, which wasm cannot express.
    Fma,
}

impl HostFunc {
    /// True when the call may change guest architectural state behind the
    /// translator's back, invalidating every cached register.
    pub fn clobbers_guest_state(self) -> bool {
        matches!(
            self,
            HostFunc::Syscall
                | HostFunc::Illegal
                | HostFunc::Breakpoint
                | HostFunc::SysRegWrite
                | HostFunc::Safepoint
        )
    }
}

/// One statement in a block. Statements execute in order.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Stmt {
    /// Define an SSA value.
    Def { dst: Value, expr: Expr },
    /// Write architectural state.
    WriteReg { reg: GuestReg, val: Value },
    /// Store to guest memory at `addr + offset`.
    Store {
        addr: Value,
        offset: i64,
        val: Value,
        sz: MemSz,
        order: Order,
    },
    /// A host call whose result is discarded.
    HostCall { func: HostFunc, args: Vec<Value> },
    /// Store the low `bytes` bytes of a vector to guest memory.
    VStore {
        addr: Value,
        offset: i64,
        val: Value,
        bytes: u32,
    },
    /// A memory fence.
    Fence,
}

impl Stmt {
    /// Visit every value this statement reads.
    pub fn for_each_use(&self, mut f: impl FnMut(Value)) {
        match self {
            Stmt::Def { expr, .. } => expr.for_each_use(f),
            Stmt::WriteReg { val, .. } => f(*val),
            Stmt::Store { addr, val, .. } => {
                f(*addr);
                f(*val);
            }
            Stmt::HostCall { args, .. } => args.iter().copied().for_each(f),
            Stmt::VStore { addr, val, .. } => {
                f(*addr);
                f(*val);
            }
            Stmt::Fence => {}
        }
    }

    /// True when removing this statement would change observable behaviour.
    pub fn has_side_effects(&self) -> bool {
        match self {
            Stmt::Def { expr, .. } => !expr.is_pure(),
            Stmt::WriteReg { .. } => true,
            Stmt::Store { .. } | Stmt::HostCall { .. } | Stmt::VStore { .. } | Stmt::Fence => true,
        }
    }
}

/// How a block transfers control.
///
/// Terminators name guest addresses, never backend labels; choosing between a
/// direct tail call, a dispatch-table lookup and an inline cache is the
/// backend's business.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Term {
    /// Continue at a statically known guest address.
    Goto { target: u64 },
    /// Two-way branch on an `I32` boolean.
    Branch {
        cond: Value,
        taken: u64,
        not_taken: u64,
    },
    /// Branch to a computed guest address (`br`, `blr`, `ret`).
    Indirect {
        target: Value,
        /// True for `ret`, which the backend may serve from a return cache.
        is_ret: bool,
    },
    /// Enter the kernel, then continue at `next`.
    Syscall { next: u64 },
    /// An instruction we cannot execute. Raises SIGILL and does not return.
    Illegal { raw: u32, pc: u64 },
}

/// A translated basic block: guest instructions from `start` up to `end`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Block {
    /// Guest address of the first instruction.
    pub start: u64,
    /// Guest address one past the last instruction.
    pub end: u64,
    /// Statements in program order.
    pub stmts: Vec<Stmt>,
    /// How the block ends.
    pub term: Term,
    /// Number of SSA values defined; value indices are `0..n_values`.
    pub n_values: u32,
    /// Where a call in this block is expected to return to.
    ///
    /// Set for `bl` and `blr`. It is not a control-flow successor — the block
    /// transfers to the callee — but it *is* an entry point, because the
    /// callee's `ret` will branch straight to it. Code discovery would miss
    /// every return site without it, and tier-2's return-address prediction
    /// (PLAN §5.6) is built on the same information.
    pub call_return: Option<u64>,
}

impl Block {
    pub fn new(start: u64) -> Block {
        Block {
            start,
            end: start,
            stmts: Vec::new(),
            term: Term::Goto { target: start },
            n_values: 0,
            call_return: None,
        }
    }

    /// Visit every value read by the terminator.
    pub fn term_uses(&self, mut f: impl FnMut(Value)) {
        match self.term {
            Term::Branch { cond, .. } => f(cond),
            Term::Indirect { target, .. } => f(target),
            _ => {}
        }
    }

    /// Statically known successors, for CFG construction. An indirect branch
    /// contributes none.
    pub fn successors(&self) -> impl Iterator<Item = u64> {
        let mut out = Vec::new();
        match self.term {
            Term::Goto { target } => out.push(target),
            Term::Branch {
                taken, not_taken, ..
            } => {
                out.push(taken);
                out.push(not_taken);
            }
            Term::Syscall { next } => out.push(next),
            Term::Indirect { .. } | Term::Illegal { .. } => {}
        }
        out.into_iter()
    }
}

/// Incrementally builds a [`Block`]. The lifter's only interface to GIR.
pub struct Builder {
    block: Block,
}

impl Builder {
    pub fn new(start: u64) -> Builder {
        Builder {
            block: Block::new(start),
        }
    }

    /// Append a value-producing expression and return its handle.
    pub fn def(&mut self, expr: Expr) -> Value {
        let v = Value(self.block.n_values);
        self.block.n_values += 1;
        self.block.stmts.push(Stmt::Def { dst: v, expr });
        v
    }

    pub fn stmt(&mut self, s: Stmt) {
        self.block.stmts.push(s);
    }

    pub fn const_i32(&mut self, v: u32) -> Value {
        self.def(Expr::ConstI32(v))
    }

    pub fn const_i64(&mut self, v: u64) -> Value {
        self.def(Expr::ConstI64(v))
    }

    /// A `ty`-typed constant. Vectors have no scalar constant form.
    pub fn const_ty(&mut self, ty: Ty, v: u64) -> Value {
        match ty {
            Ty::I32 => self.const_i32(v as u32),
            Ty::I64 => self.const_i64(v),
            Ty::V128 => {
                let mut bytes = [0u8; 16];
                bytes[..8].copy_from_slice(&v.to_le_bytes());
                self.def(Expr::VConst(Box::new(bytes)))
            }
        }
    }

    pub fn read(&mut self, reg: GuestReg) -> Value {
        self.def(Expr::ReadReg(reg))
    }

    pub fn write(&mut self, reg: GuestReg, val: Value) {
        self.stmt(Stmt::WriteReg { reg, val });
    }

    pub fn bin(&mut self, op: BinOp, ty: Ty, a: Value, b: Value) -> Value {
        self.def(Expr::Bin { op, ty, a, b })
    }

    pub fn un(&mut self, op: UnOp, ty: Ty, a: Value) -> Value {
        self.def(Expr::Un { op, ty, a })
    }

    pub fn cmp(&mut self, op: CmpOp, ty: Ty, a: Value, b: Value) -> Value {
        self.def(Expr::Cmp { op, ty, a, b })
    }

    pub fn select(&mut self, cond: Value, a: Value, b: Value, ty: Ty) -> Value {
        self.def(Expr::Select { cond, a, b, ty })
    }

    pub fn wrap(&mut self, a: Value) -> Value {
        self.def(Expr::Wrap(a))
    }

    pub fn extend(&mut self, a: Value, signed: bool) -> Value {
        self.def(Expr::Extend { a, signed })
    }

    /// Record that this block calls out and expects to resume at `pc`.
    pub fn set_call_return(&mut self, pc: u64) {
        self.block.call_return = Some(pc);
    }

    /// Mark the block as covering guest instructions up to (excluding) `pc`.
    pub fn end_at(&mut self, pc: u64) {
        self.block.end = pc;
    }

    /// The address one past the last instruction lifted so far.
    pub fn end(&self) -> u64 {
        self.block.end
    }

    pub fn finish(mut self, term: Term) -> Block {
        self.block.term = term;
        self.block
    }
}
