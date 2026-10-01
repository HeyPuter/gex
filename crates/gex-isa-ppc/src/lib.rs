//! `gex-isa-ppc` — 32-bit PowerPC instruction decoder and disassembler.
//!
//! The same contract as gex's other decoders:
//!
//! * **Total.** Every 32-bit word decodes to some [`Inst`]; what is not
//!   recognised is [`Op::Unknown`], so a block ending there raises a precise
//!   SIGILL rather than misexecuting.
//! * **Never panics**, on any word.
//!
//! The instruction set is the 32-bit user-level Power ISA with the classic
//! FPU (a 603/750-class machine, clang's `-mcpu=ppc`), plus the handful of
//! supervisor instructions a disassembler should name. 64-bit-only
//! instructions (`ld`, `rldicl`, `mulld`, …), AltiVec, SPE and VSX decode as
//! [`Op::Unknown`]: the guest is a 32-bit process on a CPU without them.
//!
//! An [`Inst`] is the opcode plus the raw word; operands are read through
//! field accessors named as the Power ISA names them, which keeps the decoder
//! a pure classifier and lets the lifter and the disassembler read exactly
//! the fields a form has.

#![forbid(unsafe_code)]

pub mod fmt;

/// Every instruction the decoder recognises. `Rc` (the record bit) and `OE`
/// (overflow enable) are not separate opcodes: see [`Inst::rc`] and
/// [`Inst::oe`].
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Op {
    Unknown,

    // -- branches and system ----------------------------------------------
    /// `b`, `ba`, `bl`, `bla`.
    B,
    /// `bc` and its link/absolute forms.
    Bc,
    /// `bclr` (`blr` and friends).
    Bclr,
    /// `bcctr` (`bctr` and friends).
    Bcctr,
    Sc,
    Tw,
    Twi,

    // -- condition register -------------------------------------------------
    Mcrf,
    Crand,
    Crandc,
    Creqv,
    Crnand,
    Crnor,
    Cror,
    Crorc,
    Crxor,
    Mcrxr,
    /// `mfcr`, or `mfocrf` when bit 11 is set.
    Mfcr,
    /// `mtcrf`, or `mtocrf` when bit 11 is set.
    Mtcrf,

    // -- integer arithmetic ---------------------------------------------
    Addi,
    Addis,
    Addic,
    /// `addic.`: a separate primary opcode, always records.
    AddicRc,
    Subfic,
    Mulli,
    Add,
    Addc,
    Adde,
    Addme,
    Addze,
    Subf,
    Subfc,
    Subfe,
    Subfme,
    Subfze,
    Neg,
    Mullw,
    Mulhw,
    Mulhwu,
    Divw,
    Divwu,

    // -- compares ---------------------------------------------------------
    Cmp,
    Cmpl,
    Cmpi,
    Cmpli,

    // -- logical ----------------------------------------------------------
    /// `andi.`, always records.
    AndiRc,
    /// `andis.`, always records.
    AndisRc,
    Ori,
    Oris,
    Xori,
    Xoris,
    And,
    Andc,
    Or,
    Orc,
    Xor,
    Nand,
    Nor,
    Eqv,
    Extsb,
    Extsh,
    Cntlzw,

    // -- rotates and shifts -------------------------------------------------
    Rlwinm,
    Rlwnm,
    Rlwimi,
    Slw,
    Srw,
    Sraw,
    Srawi,

    // -- loads and stores ---------------------------------------------------
    Lbz,
    Lbzu,
    Lbzx,
    Lbzux,
    Lhz,
    Lhzu,
    Lhzx,
    Lhzux,
    Lha,
    Lhau,
    Lhax,
    Lhaux,
    Lwz,
    Lwzu,
    Lwzx,
    Lwzux,
    Stb,
    Stbu,
    Stbx,
    Stbux,
    Sth,
    Sthu,
    Sthx,
    Sthux,
    Stw,
    Stwu,
    Stwx,
    Stwux,
    Lhbrx,
    Lwbrx,
    Sthbrx,
    Stwbrx,
    Lmw,
    Stmw,
    Lswi,
    Lswx,
    Stswi,
    Stswx,
    Lwarx,
    /// `stwcx.`, always records.
    Stwcx,

    // -- FP loads and stores ------------------------------------------------
    Lfs,
    Lfsu,
    Lfsx,
    Lfsux,
    Lfd,
    Lfdu,
    Lfdx,
    Lfdux,
    Stfs,
    Stfsu,
    Stfsx,
    Stfsux,
    Stfd,
    Stfdu,
    Stfdx,
    Stfdux,
    Stfiwx,

    // -- FP arithmetic (double, primary 63) ----------------------------------
    Fadd,
    Fsub,
    Fmul,
    Fdiv,
    Fsqrt,
    Fsel,
    Fre,
    Frsqrte,
    Fmadd,
    Fmsub,
    Fnmadd,
    Fnmsub,
    // -- FP arithmetic (single, primary 59) ----------------------------------
    Fadds,
    Fsubs,
    Fmuls,
    Fdivs,
    Fsqrts,
    Fres,
    Frsqrtes,
    Fmadds,
    Fmsubs,
    Fnmadds,
    Fnmsubs,
    // -- FP moves, conversions, compares, FPSCR ------------------------------
    Fmr,
    Fneg,
    Fabs,
    Fnabs,
    Frsp,
    Fctiw,
    Fctiwz,
    Fcmpu,
    Fcmpo,
    Mffs,
    Mtfsf,
    Mtfsfi,
    Mtfsb0,
    Mtfsb1,
    Mcrfs,

    // -- special-purpose registers, caches, barriers ---------------------------
    Mfspr,
    Mtspr,
    Mftb,
    Sync,
    Eieio,
    Isync,
    Icbi,
    Dcbz,
    Dcbf,
    Dcbst,
    Dcbt,
    Dcbtst,

    // -- supervisor: named for the disassembler, SIGILL in a Linux process ----
    Dcbi,
    Mfmsr,
    Mtmsr,
    Mfsr,
    Mfsrin,
    Mtsr,
    Mtsrin,
    Rfi,
    Tlbie,
    Tlbsync,
}

/// One decoded instruction: its opcode and raw word. Operands are read with
/// the field accessors below.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Inst {
    pub op: Op,
    pub raw: u32,
}

#[inline]
fn bits(raw: u32, first: u32, last: u32) -> u32 {
    // Power ISA bit numbering: bit 0 is the most significant.
    (raw >> (31 - last)) & ((1u32 << (last - first + 1)) - 1)
}

impl Inst {
    /// `RT`/`RS`/`FRT`/`FRS`/`BO`/`TO`/`BT`: bits 6–10.
    pub fn rt(&self) -> u8 {
        bits(self.raw, 6, 10) as u8
    }
    /// `RA`/`FRA`/`BI`/`BA`: bits 11–15.
    pub fn ra(&self) -> u8 {
        bits(self.raw, 11, 15) as u8
    }
    /// `RB`/`FRB`/`SH`/`NB`/`BB`: bits 16–20.
    pub fn rb(&self) -> u8 {
        bits(self.raw, 16, 20) as u8
    }
    /// `FRC`/`MB` (A- and M-form): bits 21–25.
    pub fn rc_field(&self) -> u8 {
        bits(self.raw, 21, 25) as u8
    }
    /// `MB` of an M-form rotate.
    pub fn mb(&self) -> u8 {
        bits(self.raw, 21, 25) as u8
    }
    /// `ME` of an M-form rotate.
    pub fn me(&self) -> u8 {
        bits(self.raw, 26, 30) as u8
    }
    /// The record bit: the instruction also sets CR0 (CR1 for FP).
    pub fn rc(&self) -> bool {
        match self.op {
            Op::AddicRc | Op::AndiRc | Op::AndisRc | Op::Stwcx => true,
            // The D-forms have no record bit; bit 31 is part of an immediate.
            Op::Addi
            | Op::Addis
            | Op::Addic
            | Op::Subfic
            | Op::Mulli
            | Op::Cmpi
            | Op::Cmpli
            | Op::Ori
            | Op::Oris
            | Op::Xori
            | Op::Xoris
            | Op::Twi
            | Op::B
            | Op::Bc
            | Op::Bclr
            | Op::Bcctr
            | Op::Sc => false,
            op if is_d_form_memory(op) => false,
            _ => self.raw & 1 != 0,
        }
    }
    /// The `OE` bit of an XO-form: the instruction also sets XER[OV,SO].
    pub fn oe(&self) -> bool {
        matches!(
            self.op,
            Op::Add
                | Op::Addc
                | Op::Adde
                | Op::Addme
                | Op::Addze
                | Op::Subf
                | Op::Subfc
                | Op::Subfe
                | Op::Subfme
                | Op::Subfze
                | Op::Neg
                | Op::Mullw
                | Op::Divw
                | Op::Divwu
        ) && bits(self.raw, 21, 21) != 0
    }
    /// A signed 16-bit immediate or displacement (`SI`, `D`).
    pub fn simm(&self) -> i32 {
        (self.raw & 0xffff) as u16 as i16 as i32
    }
    /// An unsigned 16-bit immediate (`UI`).
    pub fn uimm(&self) -> u32 {
        self.raw & 0xffff
    }
    /// `BO` of a conditional branch.
    pub fn bo(&self) -> u8 {
        self.rt()
    }
    /// `BI` of a conditional branch.
    pub fn bi(&self) -> u8 {
        self.ra()
    }
    /// `BH` of `bclr`/`bcctr` (a hint).
    pub fn bh(&self) -> u8 {
        bits(self.raw, 19, 20) as u8
    }
    /// `AA`: the branch target is absolute.
    pub fn aa(&self) -> bool {
        self.raw & 2 != 0
    }
    /// `LK`: the branch writes the link register.
    pub fn lk(&self) -> bool {
        self.raw & 1 != 0
    }
    /// `BF`: the CR field a compare (or `mcrf`, `mcrxr`, `mcrfs`) writes.
    pub fn crfd(&self) -> u8 {
        bits(self.raw, 6, 8) as u8
    }
    /// `BFA`: the CR (or FPSCR) field `mcrf`/`mcrfs` reads.
    pub fn crfs(&self) -> u8 {
        bits(self.raw, 11, 13) as u8
    }
    /// `L` of a compare: 1 is a 64-bit compare (not on this machine).
    pub fn cmp_l(&self) -> bool {
        bits(self.raw, 10, 10) != 0
    }
    /// `FXM` of `mtcrf`/`mfocrf`.
    pub fn fxm(&self) -> u8 {
        bits(self.raw, 12, 19) as u8
    }
    /// `FLM` of `mtfsf`.
    pub fn flm(&self) -> u8 {
        bits(self.raw, 7, 14) as u8
    }
    /// `U` of `mtfsfi`.
    pub fn mtfsfi_imm(&self) -> u8 {
        bits(self.raw, 16, 19) as u8
    }
    /// The special-purpose register number, with its two halves swapped back
    /// into order (the encoding stores the low five bits first).
    pub fn spr(&self) -> u32 {
        let f = bits(self.raw, 11, 20);
        ((f & 0x1f) << 5) | (f >> 5)
    }
    /// `LEV` of `sc`: 0 is a system call, anything else the hypervisor.
    pub fn lev(&self) -> u8 {
        bits(self.raw, 20, 26) as u8
    }
    /// `SR` of `mtsr`/`mfsr`.
    pub fn sr(&self) -> u8 {
        bits(self.raw, 12, 15) as u8
    }
    /// `L` of `sync` (0: `sync`/`hwsync`, 1: `lwsync`).
    pub fn sync_l(&self) -> u8 {
        bits(self.raw, 9, 10) as u8
    }
    /// The target of a `b` or `bc`, given the instruction's own address.
    /// Addresses wrap at 32 bits.
    pub fn target(&self, pc: u64) -> u64 {
        let disp: i64 = match self.op {
            Op::B => (((self.raw & 0x03ff_fffc) << 6) as i32 >> 6) as i64,
            Op::Bc => ((self.raw & 0xfffc) as u16 as i16) as i64,
            _ => 0,
        };
        let base = if self.aa() { 0 } else { pc as i64 };
        (base.wrapping_add(disp) as u64) & 0xffff_ffff
    }
}

/// The D-form loads and stores (`lwz 3, 8(4)`), whose bit 31 is part of the
/// displacement.
pub fn is_d_form_memory(op: Op) -> bool {
    use Op::*;
    matches!(
        op,
        Lbz | Lbzu
            | Lhz
            | Lhzu
            | Lha
            | Lhau
            | Lwz
            | Lwzu
            | Stb
            | Stbu
            | Sth
            | Sthu
            | Stw
            | Stwu
            | Lmw
            | Stmw
            | Lfs
            | Lfsu
            | Lfd
            | Lfdu
            | Stfs
            | Stfsu
            | Stfd
            | Stfdu
    )
}

/// Decode one instruction word. `_pc` is accepted for symmetry with gex's
/// other decoders; branch targets are computed by [`Inst::target`].
pub fn decode(raw: u32, _pc: u64) -> Inst {
    let op = classify(raw);
    let op = if reserved_ok(op, raw) { op } else { Op::Unknown };
    Inst { op, raw }
}

/// Whether the fields a form leaves unused are zero. The architecture calls
/// anything else an invalid form, with undefined results; gex makes it a
/// SIGILL rather than guess, which is also what LLVM's decoder does.
fn reserved_ok(op: Op, raw: u32) -> bool {
    use Op::*;
    let b31 = raw & 1 == 0;
    let (rt, ra, rb) = (bits(raw, 6, 10), bits(raw, 11, 15), bits(raw, 16, 20));
    match op {
        Cmp | Cmpl | Tw | Mfspr | Mtspr | Mftb | Dcbt | Dcbtst | Mfsrin | Mtsrin | Lswi | Lswx | Stswi | Stswx | Lbzx | Lbzux | Lhzx | Lhzux
        | Lhax | Lhaux | Lwzx | Lwzux | Stbx | Stbux | Sthx | Sthux | Stwx | Stwux | Lhbrx
        | Lwbrx | Sthbrx | Stwbrx | Lfsx | Lfsux | Lfdx | Lfdux | Stfsx | Stfsux | Stfdx
        | Stfdux | Stfiwx | Crand | Crandc | Creqv | Crnand | Crnor | Cror | Crorc | Crxor => b31,
        Icbi | Dcbz | Dcbf | Dcbst | Dcbi => b31 && rt == 0,
        Addme | Addze | Subfme | Subfze | Neg | Extsb | Extsh | Cntlzw => rb == 0,
        Mulhw | Mulhwu => bits(raw, 21, 21) == 0,
        Mfcr => b31 && if bits(raw, 11, 11) == 0 { bits(raw, 12, 20) == 0 } else { bits(raw, 20, 20) == 0 },
        Mtcrf => b31 && bits(raw, 20, 20) == 0,
        Mcrxr => b31 && bits(raw, 9, 20) == 0,
        Mfmsr => b31 && ra == 0 && rb == 0,
        Mtmsr | Tlbie => b31,
        Mfsr | Mtsr => b31 && bits(raw, 11, 11) == 0 && rb == 0,
        Tlbsync => b31 && rt == 0 && ra == 0 && rb == 0,
        Sync => b31 && bits(raw, 6, 8) == 0 && ra == 0 && rb == 0,
        Isync | Rfi => b31 && rt == 0 && ra == 0 && rb == 0,
        // BH is bits 19–20; 16–18 are reserved. `bcctr` cannot decrement CTR.
        Bclr => bits(raw, 16, 18) == 0,
        Bcctr => bits(raw, 16, 18) == 0 && rt & 0b00100 != 0,
        Fmr | Fneg | Fabs | Fnabs | Frsp | Fctiw | Fctiwz => ra == 0,
        Fcmpu | Fcmpo => b31 && bits(raw, 9, 10) == 0,
        Mcrfs => b31 && bits(raw, 9, 10) == 0 && bits(raw, 14, 20) == 0,
        Mffs => ra == 0 && rb == 0,
        Mtfsb0 | Mtfsb1 => ra == 0 && rb == 0,
        Mtfsfi => bits(raw, 9, 15) == 0 && bits(raw, 20, 20) == 0,
        _ => true,
    }
}

fn classify(raw: u32) -> Op {
    use Op::*;
    let primary = raw >> 26;
    let xo10 = (raw >> 1) & 0x3ff;
    let rc = raw & 1;
    match primary {
        3 => Twi,
        7 => Mulli,
        8 => Subfic,
        // L=1 is the 64-bit compare.
        10 if bits(raw, 9, 10) == 0 => Cmpli,
        11 if bits(raw, 9, 10) == 0 => Cmpi,
        12 => Addic,
        13 => AddicRc,
        14 => Addi,
        15 => Addis,
        16 => Bc,
        // LEV (bits 20–26) nonzero is a hypervisor call, which the lifter
        // refuses; bits 6–19 are ignored, as the hardware and LLVM do.
        17 if raw & 3 == 2 => Sc,
        18 => B,
        19 => match xo10 {
            0 if bits(raw, 9, 10) == 0 && bits(raw, 14, 20) == 0 && rc == 0 => Mcrf,
            16 => Bclr,
            33 => Crnor,
            50 => Rfi,
            129 => Crandc,
            150 => Isync,
            193 => Crxor,
            225 => Crnand,
            257 => Crand,
            289 => Creqv,
            417 => Crorc,
            449 => Cror,
            528 => Bcctr,
            _ => Unknown,
        },
        20 => Rlwimi,
        21 => Rlwinm,
        23 => Rlwnm,
        24 => Ori,
        25 => Oris,
        26 => Xori,
        27 => Xoris,
        28 => AndiRc,
        29 => AndisRc,
        31 => classify_31(raw),
        32 => Lwz,
        33 => Lwzu,
        34 => Lbz,
        35 => Lbzu,
        36 => Stw,
        37 => Stwu,
        38 => Stb,
        39 => Stbu,
        40 => Lhz,
        41 => Lhzu,
        42 => Lha,
        43 => Lhau,
        44 => Sth,
        45 => Sthu,
        46 => Lmw,
        47 => Stmw,
        48 => Lfs,
        49 => Lfsu,
        50 => Lfd,
        51 => Lfdu,
        52 => Stfs,
        53 => Stfsu,
        54 => Stfd,
        55 => Stfdu,
        59 => {
            let op = match (raw >> 1) & 0x1f {
                18 => Fdivs,
                20 => Fsubs,
                21 => Fadds,
                22 => Fsqrts,
                24 => Fres,
                25 => Fmuls,
                26 => Frsqrtes,
                28 => Fmsubs,
                29 => Fmadds,
                30 => Fnmsubs,
                31 => Fnmadds,
                _ => Unknown,
            };
            if a_form_reserved_ok(op, raw) { op } else { Unknown }
        }
        63 => classify_63(raw),
        _ => Unknown,
    }
}

fn classify_31(raw: u32) -> Op {
    use Op::*;
    let xo10 = (raw >> 1) & 0x3ff;
    // XO-form arithmetic: the low nine bits of the extended opcode, with OE
    // in the tenth.
    let xo9 = xo10 & 0x1ff;
    let xo_arith = match xo9 {
        8 => Some(Subfc),
        10 => Some(Addc),
        40 => Some(Subf),
        104 => Some(Neg),
        136 => Some(Subfe),
        138 => Some(Adde),
        200 => Some(Subfze),
        202 => Some(Addze),
        232 => Some(Subfme),
        234 => Some(Addme),
        235 => Some(Mullw),
        266 => Some(Add),
        459 => Some(Divwu),
        491 => Some(Divw),
        _ => None,
    };
    if let Some(op) = xo_arith {
        return op;
    }
    match xo10 {
        0 if bits(raw, 9, 10) == 0 => Cmp,
        4 => Tw,
        11 => Mulhwu,
        // mfocrf/mtocrf (bit 11) name exactly one field; more is undefined.
        19 if bits(raw, 11, 11) == 0 || (bits(raw, 12, 19) as u8).count_ones() == 1 => Mfcr,
        20 => Lwarx,
        23 => Lwzx,
        24 => Slw,
        26 => Cntlzw,
        28 => And,
        32 if bits(raw, 9, 10) == 0 => Cmpl,
        54 => Dcbst,
        55 => Lwzux,
        60 => Andc,
        75 => Mulhw,
        83 => Mfmsr,
        86 => Dcbf,
        87 => Lbzx,
        119 => Lbzux,
        124 => Nor,
        144 if bits(raw, 11, 11) == 0 || (bits(raw, 12, 19) as u8).count_ones() == 1 => Mtcrf,
        146 if bits(raw, 11, 20) == 0 => Mtmsr,
        150 if raw & 1 == 1 => Stwcx,
        151 => Stwx,
        183 => Stwux,
        210 => Mtsr,
        215 => Stbx,
        242 => Mtsrin,
        246 => Dcbtst,
        247 => Stbux,
        278 => Dcbt,
        279 => Lhzx,
        284 => Eqv,
        306 if bits(raw, 6, 15) == 0 => Tlbie,
        311 => Lhzux,
        316 => Xor,
        339 => Mfspr,
        343 => Lhax,
        371 => Mftb,
        375 => Lhaux,
        407 => Sthx,
        412 => Orc,
        439 => Sthux,
        444 => Or,
        467 => Mtspr,
        470 => Dcbi,
        476 => Nand,
        512 => Mcrxr,
        533 => Lswx,
        534 => Lwbrx,
        535 => Lfsx,
        536 => Srw,
        566 => Tlbsync,
        567 => Lfsux,
        595 => Mfsr,
        597 => Lswi,
        598 => Sync,
        599 => Lfdx,
        631 => Lfdux,
        659 => Mfsrin,
        661 => Stswx,
        662 => Stwbrx,
        663 => Stfsx,
        695 => Stfsux,
        725 => Stswi,
        727 => Stfdx,
        759 => Stfdux,
        790 => Lhbrx,
        792 => Sraw,
        824 => Srawi,
        // Bits 6–10 are `mbar`'s MO on Book E; `eieio` has them zero.
        854 if bits(raw, 6, 20) == 0 => Eieio,
        918 => Sthbrx,
        922 => Extsh,
        954 => Extsb,
        982 => Icbi,
        983 => Stfiwx,
        1014 => Dcbz,
        _ => Unknown,
    }
}

fn classify_63(raw: u32) -> Op {
    use Op::*;
    let a_form = match (raw >> 1) & 0x1f {
        18 => Some(Fdiv),
        20 => Some(Fsub),
        21 => Some(Fadd),
        22 => Some(Fsqrt),
        23 => Some(Fsel),
        24 => Some(Fre),
        25 => Some(Fmul),
        26 => Some(Frsqrte),
        28 => Some(Fmsub),
        29 => Some(Fmadd),
        30 => Some(Fnmsub),
        31 => Some(Fnmadd),
        _ => None,
    };
    if let Some(op) = a_form {
        return if a_form_reserved_ok(op, raw) { op } else { Unknown };
    }
    match (raw >> 1) & 0x3ff {
        0 => Fcmpu,
        12 => Frsp,
        14 => Fctiw,
        15 => Fctiwz,
        32 => Fcmpo,
        38 => Mtfsb1,
        40 => Fneg,
        64 => Mcrfs,
        70 => Mtfsb0,
        72 => Fmr,
        134 => Mtfsfi,
        136 => Fnabs,
        264 => Fabs,
        583 => Mffs,
        // L and W (bits 6, 15) are Power ISA 2.05 additions.
        711 if bits(raw, 6, 6) == 0 && bits(raw, 15, 15) == 0 => Mtfsf,
        _ => Unknown,
    }
}

/// Whether an A-form FP instruction's unused register fields are zero, as the
/// architecture requires: `FRC` for add/subtract/divide, `FRB` for multiply,
/// and both `FRA` and `FRC` for the one-operand forms.
fn a_form_reserved_ok(op: Op, raw: u32) -> bool {
    use Op::*;
    let (fra, frb, frc) = (bits(raw, 11, 15), bits(raw, 16, 20), bits(raw, 21, 25));
    match op {
        Fadd | Fsub | Fdiv | Fadds | Fsubs | Fdivs => frc == 0,
        Fmul | Fmuls => frb == 0,
        Fsqrt | Fsqrts | Fre | Fres | Frsqrte | Frsqrtes => fra == 0 && frc == 0,
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_follow_the_isa_numbering() {
        // lwz 0, 20(1)
        let i = decode(0x8001_0014, 0);
        assert_eq!(i.op, Op::Lwz);
        assert_eq!((i.rt(), i.ra(), i.simm()), (0, 1, 20));
        // stwu 1, -16(1)
        let i = decode(0x9421_fff0, 0);
        assert_eq!(i.op, Op::Stwu);
        assert_eq!((i.rt(), i.ra(), i.simm()), (1, 1, -16));
        // mflr 0 = mfspr 0, 8
        let i = decode(0x7c08_02a6, 0);
        assert_eq!((i.op, i.spr()), (Op::Mfspr, 8));
    }

    #[test]
    fn branch_targets_wrap_at_32_bits() {
        // b .-4
        let i = decode(0x4bff_fffc, 0);
        assert_eq!(i.op, Op::B);
        assert_eq!(i.target(0x1000), 0xffc);
        assert_eq!(i.target(0), 0xffff_fffc);
        // bc 12, 2, .+0x44
        let i = decode(0x4182_0044, 0);
        assert_eq!((i.op, i.bo(), i.bi()), (Op::Bc, 12, 2));
        assert_eq!(i.target(0x90), 0xd4);
    }

    #[test]
    fn record_and_overflow_bits() {
        // add. 8, 25, 6
        let i = decode(0x7d19_3215, 0);
        assert_eq!(i.op, Op::Add);
        assert!(i.rc() && !i.oe());
        // addo 3, 4, 5
        let i = decode(0x7c64_2e14, 0);
        assert_eq!(i.op, Op::Add);
        assert!(i.oe() && !i.rc());
        // A D-form's low bit belongs to its displacement.
        assert!(!decode(0x8001_0015, 0).rc());
    }

    #[test]
    fn sixty_four_bit_forms_are_unknown() {
        // ld 3, 0(4); rldicl 3, 3, 0, 32; cmpld 3, 4; mulld 3, 4, 5
        for raw in [0xe864_0000u32, 0x7863_0020, 0x7c23_2040, 0x7c64_29d2] {
            assert_eq!(decode(raw, 0).op, Op::Unknown, "{raw:08x}");
        }
    }
}
