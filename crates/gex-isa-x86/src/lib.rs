//! `gex-isa-x86` — x86-64 instruction decoder.
//!
//! Turns a byte sequence into a structured [`Inst`]. The invariants the rest
//! of the compiler depends on are the same as the A64 decoder's:
//!
//! * **Total.** Every byte sequence decodes to some `Inst`; what is not
//!   recognised becomes [`Op::Unknown`] with a best-effort length, so a block
//!   ending there raises a precise SIGILL rather than misexecuting.
//! * **Operands resolved at decode time.** Register widths, the high-byte
//!   registers, RIP-relative addresses (folded to absolute) and immediates
//!   (sign-extended to their operand size) all arrive final.
//! * **Never panics** on any input, including truncated ones: a sequence cut
//!   short by the end of executable memory is `Unknown`.
//!
//! 64-bit mode and 32-bit compatibility mode ([`Mode`]) are decoded.
//! VEX/EVEX-encoded instructions are recognised far enough to measure their
//! length and are otherwise `Unknown`: the CPUID gex reports advertises no
//! AVX, so a correct guest never runs them.

#![forbid(unsafe_code)]

pub mod decode;
pub mod fmt;

pub use decode::decode;

/// Longest legal x86 instruction.
pub const MAX_INST_BYTES: usize = 15;

/// The processor mode the bytes are decoded in. gex runs a 64-bit Linux
/// process, but a code segment inside it may be a 32-bit one -- a WoW64
/// Wine's guest, or an i386 program -- and the same opcode bytes mean
/// different things there: no REX, 32-bit default operand and stack widths,
/// `mod=00 r/m=101` an absolute disp32 rather than RIP-relative, and every
/// effective address wrapped at 32 bits.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    /// 64-bit long mode.
    Long,
    /// 32-bit compatibility (legacy protected) mode.
    Compat,
}

static MODE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

std::thread_local! {
    /// This thread's mode while a [`ModeScope`] is open: 0 none (the
    /// process default holds), else `1 + Mode as u8`.
    static SCOPED: core::cell::Cell<u8> = const { core::cell::Cell::new(0) };
}

/// Set the process default every following `decode` uses: a whole i386
/// program is Compat. Code whose mode is its own -- an image of the other
/// class in a WoW64 process -- is decoded under [`set_thread_mode`] or a
/// [`ModeScope`] instead. Clears this thread's own mode.
pub fn set_mode(m: Mode) {
    MODE.store(m as u8, core::sync::atomic::Ordering::Relaxed);
    SCOPED.with(|s| s.set(0));
}

/// Set this thread's mode until the next call (or `set_mode` on this thread),
/// whatever the process default is: what translating one image does, so two
/// threads translating images of different classes each decode their own.
pub fn set_thread_mode(m: Mode) {
    SCOPED.with(|s| s.set(1 + m as u8));
}

/// The mode `decode` assumes on this thread: an open [`ModeScope`]'s, or the
/// process default.
pub fn mode() -> Mode {
    let v = match SCOPED.with(|s| s.get()) {
        0 => MODE.load(core::sync::atomic::Ordering::Relaxed),
        v => v - 1,
    };
    if v == 0 {
        Mode::Long
    } else {
        Mode::Compat
    }
}

/// The process default, whatever scope this thread has open.
pub fn default_mode() -> Mode {
    if MODE.load(core::sync::atomic::Ordering::Relaxed) == 0 {
        Mode::Long
    } else {
        Mode::Compat
    }
}

/// Decode in `mode` on this thread until the scope is dropped, whatever the
/// process default is. Scopes nest; dropping one restores the mode the
/// thread had before it. Two threads translating code of different classes
/// each see their own.
#[must_use = "the mode holds only while the scope is alive"]
pub struct ModeScope {
    prev: u8,
}

impl ModeScope {
    pub fn enter(mode: Mode) -> ModeScope {
        ModeScope { prev: SCOPED.with(|s| s.replace(1 + mode as u8)) }
    }
}

impl Drop for ModeScope {
    fn drop(&mut self) {
        SCOPED.with(|s| s.set(self.prev));
    }
}

/// Operand width.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum Width {
    B,
    W,
    D,
    Q,
}

impl Width {
    #[inline]
    pub fn bits(self) -> u32 {
        match self {
            Width::B => 8,
            Width::W => 16,
            Width::D => 32,
            Width::Q => 64,
        }
    }
    #[inline]
    pub fn bytes(self) -> u32 {
        self.bits() / 8
    }
    /// All-ones mask for the width.
    #[inline]
    pub fn mask(self) -> u64 {
        match self {
            Width::Q => u64::MAX,
            w => (1u64 << w.bits()) - 1,
        }
    }
    #[inline]
    pub fn from_bytes(n: u32) -> Width {
        match n {
            1 => Width::B,
            2 => Width::W,
            4 => Width::D,
            _ => Width::Q,
        }
    }
}

/// Segment registers, by hardware number.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Seg {
    Es = 0,
    Cs = 1,
    Ss = 2,
    Ds = 3,
    Fs = 4,
    Gs = 5,
}

impl Seg {
    pub fn from_num(n: u8) -> Option<Seg> {
        Some(match n {
            0 => Seg::Es,
            1 => Seg::Cs,
            2 => Seg::Ss,
            3 => Seg::Ds,
            4 => Seg::Fs,
            5 => Seg::Gs,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        ["es", "cs", "ss", "ds", "fs", "gs"][self as usize]
    }
}

/// A register operand.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Reg {
    /// General-purpose register `n` (hardware numbering: rax, rcx, rdx, rbx,
    /// rsp, rbp, rsi, rdi, r8–r15) viewed at `Width`. The 8-bit view of
    /// registers 4–7 is `spl`/`bpl`/`sil`/`dil`; see [`Reg::High`].
    Gp(u8, Width),
    /// The legacy high-byte registers `ah`, `ch`, `dh`, `bh` (`n` = 0..=3
    /// naming rax..rbx).
    High(u8),
    /// `xmm0`–`xmm15`.
    Xmm(u8),
    /// `mm0`–`mm7`.
    Mm(u8),
    /// x87 `st(i)`, stack-relative.
    St(u8),
    /// A segment register.
    Seg(Seg),
}

/// A memory operand. The effective address is
/// `seg_base + base + index * scale + disp`, computed at the address width.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Mem {
    /// Base general-purpose register, hardware numbering.
    pub base: Option<u8>,
    pub index: Option<u8>,
    /// 1, 2, 4 or 8.
    pub scale: u8,
    /// Displacement. For a RIP-relative operand this already holds the
    /// absolute address (`next_pc + disp32`) and `rip` is set.
    pub disp: i64,
    /// True when `disp` came from a RIP-relative encoding (disassembly only).
    pub rip: bool,
    /// A segment override that matters: `fs` or `gs`. The others have a base
    /// of zero in 64-bit mode and are dropped.
    pub seg: Option<Seg>,
    /// The `0x67` prefix: address computed at 32 bits.
    pub addr32: bool,
}

impl Mem {
    pub const fn abs(disp: i64) -> Mem {
        Mem {
            base: None,
            index: None,
            scale: 1,
            disp,
            rip: false,
            seg: None,
            addr32: false,
        }
    }
}

/// An instruction operand.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Operand {
    Reg(Reg),
    Mem(Mem),
    /// An immediate, already sign- or zero-extended as the instruction
    /// defines. Branch targets are absolute addresses here.
    Imm(i64),
}

/// A condition code, by hardware number: `o, no, b, ae, e, ne, be, a, s,
/// ns, p, np, l, ge, le, g`.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct Cond(pub u8);

impl Cond {
    pub fn name(self) -> &'static str {
        [
            "o", "no", "b", "ae", "e", "ne", "be", "a", "s", "ns", "p", "np", "l", "ge", "le", "g",
        ][(self.0 & 15) as usize]
    }
    /// The condition true exactly when this one is false.
    #[inline]
    pub fn invert(self) -> Cond {
        Cond(self.0 ^ 1)
    }
}

/// `rep`/`repe` and `repne` prefixes, as they apply to string instructions.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Default)]
pub enum Rep {
    #[default]
    None,
    /// `f3`: `rep` for movs/stos/lods, `repe` for cmps/scas.
    Rep,
    /// `f2`: `repne` for cmps/scas.
    RepNe,
}

/// The rounding mode of an SSE4.1 `round*` immediate, or "use MXCSR".
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum RoundMode {
    Nearest,
    Down,
    Up,
    Truncate,
    Mxcsr,
}

/// Which registers an x87 two-operand instruction names: `st(0)` and
/// `st(i)`, in either order, or `st(0)` and memory.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum X87Src {
    /// `st(0) op= m32` / `m64` (float), width says which.
    MemF(Width),
    /// `st(0) op= m16` / `m32` (integer).
    MemI(Width),
    /// `st(0) op= st(i)` — result to `st(0)`.
    St0Sti(u8),
    /// `st(i) op= st(0)` — result to `st(i)`; `pop` when the encoding pops.
    StiSt0 { i: u8, pop: bool },
}

/// x87 memory load/store sizes.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum X87Mem {
    F32,
    F64,
    F80,
    I16,
    I32,
    I64,
}

/// Operation. One variant per mnemonic (with the condition, width or lane
/// type folded in where an instruction family shares an encoding).
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Op {
    /// Not an instruction gex knows. `len` in the [`Inst`] is best-effort.
    Unknown,
    /// `ud2` and other architecturally undefined encodings.
    Undefined,

    // -- data movement --------------------------------------------------
    Mov,
    /// `mov r64, imm64`.
    Movabs,
    /// Zero-/sign-extend from the given source width to the operand width.
    Movzx(Width),
    Movsx(Width),
    Movsxd,
    Lea,
    Push,
    Pop,
    Xchg,
    Cmovcc(Cond),
    Setcc(Cond),
    /// `cbw`/`cwde`/`cdqe`: sign-extend the low half of rax into the width.
    Cbw,
    /// `cwd`/`cdq`/`cqo`: sign-extend rax into rdx:rax at the width.
    Cwd,
    Bswap,
    Movbe,
    Xlat,

    // -- arithmetic ------------------------------------------------------
    Add,
    Or,
    Adc,
    Sbb,
    And,
    Sub,
    Xor,
    Cmp,
    Test,
    Inc,
    Dec,
    Neg,
    Not,
    /// One-operand unsigned multiply into rdx:rax.
    Mul,
    /// One-operand signed multiply into rdx:rax.
    Imul1,
    /// Two- and three-operand signed multiply; the truncated form.
    Imul,
    Div,
    Idiv,
    Shl,
    Shr,
    Sar,
    Rol,
    Ror,
    Rcl,
    Rcr,
    Shld,
    Shrd,
    Bt,
    Bts,
    Btr,
    Btc,
    Bsf,
    Bsr,
    Tzcnt,
    Lzcnt,
    Popcnt,
    Xadd,
    Cmpxchg,
    /// `cmpxchg8b`/`cmpxchg16b`, width says which.
    CmpxchgNb,
    Crc32,
    // BMI (decoded for completeness; not advertised)
    Andn,
    Bzhi,
    Blsr,
    Blsmsk,
    Blsi,
    Sarx,
    Shlx,
    Shrx,
    Bextr,

    // -- control flow ----------------------------------------------------
    Jmp,
    Jcc(Cond),
    /// `jrcxz`/`jecxz`.
    Jrcxz,
    Loop,
    Loope,
    Loopne,
    Call,
    Ret,
    /// Far `jmp`/`call` (`ea`, `9a`, `ff /5`, `ff /3`): a selector and an
    /// offset, the width the offset's. Either two immediates (offset, then
    /// selector; 32-bit code only) or one memory operand holding the offset
    /// and then the selector. A WoW64 process changes mode through them.
    JmpFar,
    CallFar,
    /// `retf` (`cb`, `ca imm16`): pops the offset and the selector at the
    /// width, then the immediate's bytes.
    RetFar,
    /// `iret`/`iretd`/`iretq`: pops the offset, the selector and the flags at
    /// the width; in 64-bit mode the stack pointer and the stack segment too.
    Iret,
    Leave,
    Enter,
    /// `pusha`/`pushad` and `popa`/`popad`: 32-bit code only.
    Pusha,
    Popa,
    Syscall,
    Int3,
    Int,
    Hlt,
    /// An I/O-privileged instruction, #GP in user mode (IOPL 0): `cli`,
    /// `sti`, `in`, `out`, `ins`, `outs`. Carries the opcode byte.
    Privileged(u8),
    Nop,
    Endbr,
    Pause,
    Cpuid,
    Rdtsc,
    Rdtscp,
    Xgetbv,
    Rdrand,
    Rdseed,
    Fence,
    Prefetch,
    Clflush,
    Xsave,
    Xrstor,
    Fxsave,
    Fxrstor,

    // -- flags -----------------------------------------------------------
    Clc,
    Stc,
    Cmc,
    Cld,
    Std,
    Sahf,
    Lahf,
    Pushf,
    Popf,

    // -- strings (width in `Inst::width`) ----------------------------------
    Movs,
    Stos,
    Lods,
    Scas,
    Cmps,

    // -- SSE data movement -----------------------------------------------
    /// `movaps`/`movapd`/`movdqa`: aligned 128-bit move.
    Movaps,
    /// `movups`/`movupd`/`movdqu`/`lddqu`/`movntdqa`: unaligned 128-bit move.
    Movups,
    /// `movss` — 32-bit scalar; a register destination from memory zeroes the
    /// upper lanes, register-to-register merges.
    Movss,
    /// `movsd` scalar double; same merging rule.
    Movsd,
    /// `movd`/`movq` between a GPR or memory and an xmm/mm register; the
    /// width says 32 or 64 bits. Zero-extends into the vector.
    Movd,
    /// `movq xmm, xmm/m64`: low 64 bits, upper zeroed.
    Movq,
    /// `movlps`/`movlpd`: low 64 bits, upper preserved (load) / store low.
    Movlps,
    /// `movhps`/`movhpd`: high 64 bits from/to memory.
    Movhps,
    /// `movhlps`: high qword of src into low qword of dst.
    Movhlps,
    /// `movlhps`: low qword of src into high qword of dst.
    Movlhps,
    /// `movmskps`/`movmskpd`: lane sign bits to a GPR. Lane in `lane`.
    Movmsk,
    /// `pmovmskb`.
    Pmovmskb,
    /// `movnt*` non-temporal stores behave as ordinary stores.
    Movnti,
    /// `movsldup`/`movshdup`/`movddup`.
    Movsldup,
    Movshdup,
    Movddup,
    Maskmov,

    // -- SSE floating point (lane: F32/F64; scalar flag) ------------------
    Addp,
    Subp,
    Mulp,
    Divp,
    Minp,
    Maxp,
    Sqrtp,
    Rsqrtp,
    Rcpp,
    /// `and/andn/or/xor ps/pd` — bitwise on the whole register.
    Andp,
    Andnp,
    Orp,
    Xorp,
    /// `cmpps` etc. with the predicate immediate.
    Cmpp,
    /// `comiss`/`comisd`/`ucomiss`/`ucomisd`: compare into ZF/PF/CF.
    Comis,
    Ucomis,
    Haddp,
    Hsubp,
    Addsubp,
    Roundp,
    Dpp,
    Blendp,
    Blendvp,
    Shufp,
    Unpcklp,
    Unpckhp,

    // -- conversions -----------------------------------------------------
    /// `cvtsi2ss`/`cvtsi2sd`: integer GPR to scalar float.
    Cvtsi2s,
    /// `cvtss2si`/`cvtsd2si` (round per MXCSR) and the truncating `cvtt*`.
    Cvts2si { truncate: bool },
    /// `cvtss2sd`/`cvtsd2ss`.
    Cvts2s,
    /// `cvtps2pd`/`cvtpd2ps`.
    Cvtp2p,
    /// `cvtdq2ps`/`cvtdq2pd`.
    Cvtdq2p,
    /// `cvtps2dq`/`cvtpd2dq` and the truncating forms.
    Cvtp2dq { truncate: bool },
    /// MMX-register conversions (`cvtpi2ps` etc.).
    /// `cvtpi2ps`/`cvtpi2pd` (an `xmm` destination) and `cvt[t]ps2pi`/
    /// `cvt[t]pd2pi` (an `mm` one).
    CvtMmx { truncate: bool },

    // -- SSE integer (lane in `lane`) -------------------------------------
    Padd,
    Psub,
    Padds,
    Paddus,
    Psubs,
    Psubus,
    Pmullw,
    Pmulhw,
    Pmulhuw,
    Pmulhrsw,
    Pmulld,
    Pmuludq,
    Pmuldq,
    Pmaddwd,
    Pmaddubsw,
    Pavg,
    Pmaxu,
    Pminu,
    Pmaxs,
    Pmins,
    Psadbw,
    Pcmpeq,
    Pcmpgt,
    Pand,
    Pandn,
    Por,
    Pxor,
    /// Shifts by immediate or by the low qword of an xmm/m: `lane` and the
    /// direction/sign in the op.
    Psll,
    Psrl,
    Psra,
    /// Whole-register byte shifts.
    Pslldq,
    Psrldq,
    Packss,
    Packus,
    Punpckl,
    Punpckh,
    Pshufd,
    Pshufhw,
    Pshuflw,
    Pshufw,
    Pshufb,
    Palignr,
    Pabs,
    Psign,
    Phadd,
    Phadds,
    Phsub,
    Phsubs,
    Pblendvb,
    Pblendw,
    Ptest,
    Pextr,
    Pinsr,
    Extractps,
    Insertps,
    /// `pmovsx*`/`pmovzx*`: widen the low lanes from the first lane type to
    /// the second.
    Pmovsx(Lane, Lane),
    Pmovzx(Lane, Lane),
    Phminposuw,
    Pcmpistri,
    Pcmpistrm,
    Pcmpestri,
    Pcmpestrm,
    Pclmulqdq,
    Mpsadbw,
    Emms,
    Ldmxcsr,
    Stmxcsr,

    // -- x87 -------------------------------------------------------------
    /// Load from memory and push.
    Fld(X87Mem),
    /// `fild`: integer load and push. `X87Mem` is I16/I32/I64.
    Fild(X87Mem),
    /// Store `st(0)` to memory, popping when `pop`.
    Fst { mem: X87Mem, pop: bool },
    /// `fist`/`fistp`/`fisttp`: integer store, optionally popping or truncating.
    Fist { mem: X87Mem, pop: bool, truncate: bool },
    /// `fld st(i)`: push a copy.
    FldSt(u8),
    /// `fst`/`fstp st(i)`.
    FstSt { i: u8, pop: bool },
    Fxch(u8),
    Fadd(X87Src),
    Fsub(X87Src),
    /// `fsubr`: reversed subtraction.
    Fsubr(X87Src),
    Fmul(X87Src),
    Fdiv(X87Src),
    Fdivr(X87Src),
    /// `fcom`/`fcomp`/`fcompp` — compare setting C0/C2/C3.
    Fcom { src: X87Src, pop: u8 },
    /// `fucom`/`fucomp`/`fucompp`.
    Fucom { i: u8, pop: u8 },
    /// `fcomi`/`fcomip`/`fucomi`/`fucomip`: compare into ZF/PF/CF.
    Fcomi { i: u8, pop: bool, unordered: bool },
    /// `ficom`/`ficomp`.
    Ficom { mem: X87Mem, pop: bool },
    Fchs,
    Fabs,
    Ftst,
    Fxam,
    Fld1,
    Fldl2t,
    Fldl2e,
    Fldpi,
    Fldlg2,
    Fldln2,
    Fldz,
    F2xm1,
    Fyl2x,
    Fptan,
    Fpatan,
    Fxtract,
    Fprem1,
    Fdecstp,
    Fincstp,
    Fprem,
    Fyl2xp1,
    Fsqrt,
    Fsincos,
    Frndint,
    Fscale,
    Fsin,
    Fcos,
    Fnop,
    Fninit,
    Fnclex,
    Fldcw,
    Fnstcw,
    Fnstsw,
    /// `fnstsw ax`.
    FnstswAx,
    Fldenv,
    Fnstenv,
    Frstor,
    Fnsave,
    /// `fbld`: push an 18-digit packed BCD integer (10 bytes).
    Fbld,
    /// `fbstp`: store `st0` as packed BCD, rounded by RC, and pop.
    Fbstp,
    Ffree(u8),
    Ffreep(u8),
    Fcmovcc { cond: Cond, i: u8 },
    Fwait,
}

/// The lane type of a SIMD operation.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Lane {
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
}

impl Lane {
    pub fn bytes(self) -> u32 {
        match self {
            Lane::I8 => 1,
            Lane::I16 => 2,
            Lane::I32 | Lane::F32 => 4,
            Lane::I64 | Lane::F64 => 8,
        }
    }
}

/// A decoded instruction.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Inst {
    pub op: Op,
    /// Encoded length in bytes, 1..=15.
    pub len: u8,
    /// Operand width for integer operations; for SSE ops the scalar width
    /// (`Movd`) or the element size of the mask.
    pub width: Width,
    /// SIMD lane type where the op has one.
    pub lane: Lane,
    /// True for a scalar SSE operation (`addss`, `sqrtsd`, …) that touches
    /// only the low lane and preserves the rest of the destination.
    pub scalar: bool,
    /// True when the SIMD operand is an `mm` (64-bit) register form.
    pub mmx: bool,
    pub rep: Rep,
    pub lock: bool,
    /// Operands in Intel order: destination first.
    pub ops: [Option<Operand>; 3],
}

impl Inst {
    pub fn n_ops(&self) -> usize {
        self.ops.iter().filter(|o| o.is_some()).count()
    }
    pub fn op0(&self) -> Operand {
        self.ops[0].expect("operand 0")
    }
    pub fn op1(&self) -> Operand {
        self.ops[1].expect("operand 1")
    }
    pub fn op2(&self) -> Operand {
        self.ops[2].expect("operand 2")
    }
    /// True when this instruction could be the start of a real instruction
    /// gex can name, as opposed to padding or data.
    pub fn is_known(&self) -> bool {
        !matches!(self.op, Op::Unknown | Op::Undefined)
    }
}
