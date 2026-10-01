//! Disassembly in LLVM's spelling (`llvm-objdump --mcpu=ppc`): bare register
//! numbers, the extended mnemonics LLVM prints (`li`, `mr`, `slwi`, `bt`,
//! `bdnz`, `blr`, …) and branch targets as absolute addresses. Matching LLVM
//! is what lets the objdump-diff test check the decoder against a real
//! toolchain word for word.

use crate::{Inst, Op};

/// The text of one instruction at `pc`.
pub fn disasm(inst: &Inst, pc: u64) -> String {
    let i = inst;
    let dot = if i.rc() { "." } else { "" };
    let (rt, ra, rb) = (i.rt(), i.ra(), i.rb());
    let d = |i: &Inst| format!("{}({})", i.simm(), i.ra());
    let name = mnemonic(i.op);
    use Op::*;
    match i.op {
        Unknown => "<unknown>".into(),

        // -- branches ---------------------------------------------------------
        B => {
            let m = match (i.lk(), i.aa()) {
                (false, false) => "b",
                (true, false) => "bl",
                (false, true) => "ba",
                (true, true) => "bla",
            };
            format!("{m} {:#x}", i.target(pc))
        }
        Bc => bc_text(i, pc),
        Bclr | Bcctr => bclr_text(i),
        Sc if i.lev() != 0 => format!("sc {}", i.lev()),
        Sc => "sc".into(),
        Tw => {
            if rt == 31 && ra == 0 && rb == 0 {
                "trap".into()
            } else if let Some(c) = trap_cond(rt) {
                format!("tw{c} {ra}, {rb}")
            } else {
                format!("tw {rt}, {ra}, {rb}")
            }
        }
        Twi => match trap_cond(rt) {
            Some(c) => format!("tw{c}i {ra}, {}", i.simm()),
            None => format!("twi {rt}, {ra}, {}", i.simm()),
        },

        // -- condition register -------------------------------------------------
        Mcrf => format!("mcrf {}, {}", i.crfd(), i.crfs()),
        Crxor if rt == ra && ra == rb => format!("crclr {rt}"),
        Creqv if rt == ra && ra == rb => format!("crset {rt}"),
        Cror if ra == rb => format!("crmove {rt}, {ra}"),
        Crnor if ra == rb => format!("crnot {rt}, {ra}"),
        Crand | Crandc | Creqv | Crnand | Crnor | Cror | Crorc | Crxor => {
            format!("{name} {rt}, {ra}, {rb}")
        }
        Mcrxr => format!("mcrxr {}", i.crfd()),
        Mfcr => {
            if i.raw & (1 << 20) != 0 {
                format!("mfocrf {rt}, {}", i.fxm())
            } else {
                format!("mfcr {rt}")
            }
        }
        Mtcrf => {
            if i.raw & (1 << 20) != 0 {
                format!("mtocrf {}, {rt}", i.fxm())
            } else if i.fxm() == 0xff {
                format!("mtcr {rt}")
            } else {
                format!("mtcrf {}, {rt}", i.fxm())
            }
        }

        // -- integer arithmetic ---------------------------------------------
        Addi if ra == 0 => format!("li {rt}, {}", i.simm()),
        Addis if ra == 0 => format!("lis {rt}, {}", i.simm()),
        Addi | Addis | Addic | Subfic | Mulli => format!("{name} {rt}, {ra}, {}", i.simm()),
        AddicRc => format!("addic. {rt}, {ra}, {}", i.simm()),
        Subf | Subfc if !i.oe() => {
            let m = if i.op == Subf { "sub" } else { "subc" };
            format!("{m}{dot} {rt}, {rb}, {ra}")
        }
        Add | Addc | Adde | Subf | Subfc | Subfe | Mullw | Divw | Divwu | Mulhw | Mulhwu => {
            let o = if i.oe() { "o" } else { "" };
            format!("{name}{o}{dot} {rt}, {ra}, {rb}")
        }
        Addme | Addze | Subfme | Subfze | Neg => {
            let o = if i.oe() { "o" } else { "" };
            format!("{name}{o}{dot} {rt}, {ra}")
        }

        // -- compares ---------------------------------------------------------
        Cmp | Cmpl | Cmpi | Cmpli => {
            let m = match i.op {
                Cmp => "cmpw",
                Cmpl => "cmplw",
                Cmpi => "cmpwi",
                _ => "cmplwi",
            };
            let crf = if i.crfd() == 0 {
                String::new()
            } else {
                format!("{}, ", i.crfd())
            };
            match i.op {
                Cmp | Cmpl => format!("{m} {crf}{ra}, {rb}"),
                Cmpi => format!("{m} {crf}{ra}, {}", i.simm()),
                _ => format!("{m} {crf}{ra}, {}", i.uimm()),
            }
        }

        // -- logical ----------------------------------------------------------
        Ori if rt == 0 && ra == 0 && i.uimm() == 0 => "nop".into(),
        AndiRc | AndisRc | Ori | Oris | Xori | Xoris => {
            // D-form logicals put RS in the RT position and RA second.
            format!("{name} {ra}, {rt}, {}", i.uimm())
        }
        Or if rt == rb => format!("mr{dot} {ra}, {rt}"),
        Nor if rt == rb => format!("not{dot} {ra}, {rt}"),
        And | Andc | Or | Orc | Xor | Nand | Nor | Eqv | Slw | Srw | Sraw => {
            format!("{name}{dot} {ra}, {rt}, {rb}")
        }
        Extsb | Extsh | Cntlzw => format!("{name}{dot} {ra}, {rt}"),
        Srawi => format!("srawi{dot} {ra}, {rt}, {rb}"),

        // -- rotates ------------------------------------------------------------
        Rlwinm => rlwinm_text(i),
        Rlwnm => {
            let (mb, me) = (i.mb(), i.me());
            if mb == 0 && me == 31 {
                format!("rotlw{dot} {ra}, {rt}, {rb}")
            } else {
                format!("rlwnm{dot} {ra}, {rt}, {rb}, {mb}, {me}")
            }
        }
        Rlwimi => format!("rlwimi{dot} {ra}, {rt}, {rb}, {}, {}", i.mb(), i.me()),

        // -- loads and stores ---------------------------------------------------
        Lbz | Lbzu | Lhz | Lhzu | Lha | Lhau | Lwz | Lwzu | Stb | Stbu | Sth | Sthu | Stw
        | Stwu | Lmw | Stmw | Lfs | Lfsu | Lfd | Lfdu | Stfs | Stfsu | Stfd | Stfdu => {
            format!("{name} {rt}, {}", d(i))
        }
        // Bit 31 is the Power ISA 2.05 exclusive-access hint.
        Lwarx if i.raw & 1 != 0 => format!("lwarx {rt}, {ra}, {rb}, 1"),
        Lbzx | Lbzux | Lhzx | Lhzux | Lhax | Lhaux | Lwzx | Lwzux | Stbx | Stbux | Sthx
        | Sthux | Stwx | Stwux | Lhbrx | Lwbrx | Sthbrx | Stwbrx | Lswx | Stswx | Lwarx
        | Lfsx | Lfsux | Lfdx | Lfdux | Stfsx | Stfsux | Stfdx | Stfdux | Stfiwx => {
            format!("{name} {rt}, {ra}, {rb}")
        }
        Stwcx => format!("stwcx. {rt}, {ra}, {rb}"),
        Lswi | Stswi => format!("{name} {rt}, {ra}, {rb}"),

        // -- floating point -------------------------------------------------------
        Fadd | Fsub | Fdiv | Fadds | Fsubs | Fdivs => {
            format!("{name}{dot} {rt}, {ra}, {rb}")
        }
        Fmul | Fmuls => format!("{name}{dot} {rt}, {ra}, {}", i.rc_field()),
        Fmadd | Fmsub | Fnmadd | Fnmsub | Fmadds | Fmsubs | Fnmadds | Fnmsubs | Fsel => {
            format!("{name}{dot} {rt}, {ra}, {}, {rb}", i.rc_field())
        }
        Fsqrt | Fsqrts | Fre | Fres | Frsqrte | Frsqrtes | Fmr | Fneg | Fabs | Fnabs | Frsp
        | Fctiw | Fctiwz => format!("{name}{dot} {rt}, {rb}"),
        Fcmpu | Fcmpo => format!("{name} {}, {ra}, {rb}", i.crfd()),
        Mffs => format!("mffs{dot} {rt}"),
        Mtfsf => format!("mtfsf{dot} {}, {rb}", i.flm()),
        Mtfsfi => format!("mtfsfi{dot} {}, {}", i.crfd(), i.mtfsfi_imm()),
        Mtfsb0 | Mtfsb1 => format!("{name}{dot} {rt}"),
        Mcrfs => format!("mcrfs {}, {}", i.crfd(), i.crfs()),

        // -- SPRs, caches, barriers -----------------------------------------------
        Mfspr => match i.spr() {
            1 => format!("mfxer {rt}"),
            8 => format!("mflr {rt}"),
            9 => format!("mfctr {rt}"),
            spr => format!("mfspr {rt}, {spr}"),
        },
        Mtspr => match i.spr() {
            1 => format!("mtxer {rt}"),
            8 => format!("mtlr {rt}"),
            9 => format!("mtctr {rt}"),
            spr => format!("mtspr {spr}, {rt}"),
        },
        Mftb => match i.spr() {
            268 => format!("mftb {rt}"),
            269 => format!("mftbu {rt}"),
            spr => format!("mftb {rt}, {spr}"),
        },
        Sync => match i.sync_l() {
            0 => "sync".into(),
            1 => "lwsync".into(),
            l => format!("sync {l}"),
        },
        Eieio => "eieio".into(),
        Isync => "isync".into(),
        // The touch hints' TH (bits 6–10) is printed when set.
        Dcbt | Dcbtst if rt != 0 => format!("{name} {ra}, {rb}, {rt}"),
        Icbi | Dcbz | Dcbf | Dcbst | Dcbt | Dcbtst | Dcbi => format!("{name} {ra}, {rb}"),

        // -- supervisor ---------------------------------------------------------
        Mfmsr => format!("mfmsr {rt}"),
        Mtmsr => format!("mtmsr {rt}"),
        Mfsr => format!("mfsr {rt}, {}", i.sr()),
        Mtsr => format!("mtsr {}, {rt}", i.sr()),
        Mfsrin => format!("mfsrin {rt}, {rb}"),
        Mtsrin => format!("mtsrin {rt}, {rb}"),
        Rfi => "rfi".into(),
        Tlbie => format!("tlbie {rb}"),
        Tlbsync => "tlbsync".into(),
    }
}

/// The base mnemonic of an opcode, without record or overflow suffixes.
pub fn mnemonic(op: Op) -> &'static str {
    use Op::*;
    match op {
        Unknown => "<unknown>",
        B => "b",
        Bc => "bc",
        Bclr => "bclr",
        Bcctr => "bcctr",
        Sc => "sc",
        Tw => "tw",
        Twi => "twi",
        Mcrf => "mcrf",
        Crand => "crand",
        Crandc => "crandc",
        Creqv => "creqv",
        Crnand => "crnand",
        Crnor => "crnor",
        Cror => "cror",
        Crorc => "crorc",
        Crxor => "crxor",
        Mcrxr => "mcrxr",
        Mfcr => "mfcr",
        Mtcrf => "mtcrf",
        Addi => "addi",
        Addis => "addis",
        Addic => "addic",
        AddicRc => "addic.",
        Subfic => "subfic",
        Mulli => "mulli",
        Add => "add",
        Addc => "addc",
        Adde => "adde",
        Addme => "addme",
        Addze => "addze",
        Subf => "subf",
        Subfc => "subfc",
        Subfe => "subfe",
        Subfme => "subfme",
        Subfze => "subfze",
        Neg => "neg",
        Mullw => "mullw",
        Mulhw => "mulhw",
        Mulhwu => "mulhwu",
        Divw => "divw",
        Divwu => "divwu",
        Cmp => "cmpw",
        Cmpl => "cmplw",
        Cmpi => "cmpwi",
        Cmpli => "cmplwi",
        AndiRc => "andi.",
        AndisRc => "andis.",
        Ori => "ori",
        Oris => "oris",
        Xori => "xori",
        Xoris => "xoris",
        And => "and",
        Andc => "andc",
        Or => "or",
        Orc => "orc",
        Xor => "xor",
        Nand => "nand",
        Nor => "nor",
        Eqv => "eqv",
        Extsb => "extsb",
        Extsh => "extsh",
        Cntlzw => "cntlzw",
        Rlwinm => "rlwinm",
        Rlwnm => "rlwnm",
        Rlwimi => "rlwimi",
        Slw => "slw",
        Srw => "srw",
        Sraw => "sraw",
        Srawi => "srawi",
        Lbz => "lbz",
        Lbzu => "lbzu",
        Lbzx => "lbzx",
        Lbzux => "lbzux",
        Lhz => "lhz",
        Lhzu => "lhzu",
        Lhzx => "lhzx",
        Lhzux => "lhzux",
        Lha => "lha",
        Lhau => "lhau",
        Lhax => "lhax",
        Lhaux => "lhaux",
        Lwz => "lwz",
        Lwzu => "lwzu",
        Lwzx => "lwzx",
        Lwzux => "lwzux",
        Stb => "stb",
        Stbu => "stbu",
        Stbx => "stbx",
        Stbux => "stbux",
        Sth => "sth",
        Sthu => "sthu",
        Sthx => "sthx",
        Sthux => "sthux",
        Stw => "stw",
        Stwu => "stwu",
        Stwx => "stwx",
        Stwux => "stwux",
        Lhbrx => "lhbrx",
        Lwbrx => "lwbrx",
        Sthbrx => "sthbrx",
        Stwbrx => "stwbrx",
        Lmw => "lmw",
        Stmw => "stmw",
        Lswi => "lswi",
        Lswx => "lswx",
        Stswi => "stswi",
        Stswx => "stswx",
        Lwarx => "lwarx",
        Stwcx => "stwcx.",
        Lfs => "lfs",
        Lfsu => "lfsu",
        Lfsx => "lfsx",
        Lfsux => "lfsux",
        Lfd => "lfd",
        Lfdu => "lfdu",
        Lfdx => "lfdx",
        Lfdux => "lfdux",
        Stfs => "stfs",
        Stfsu => "stfsu",
        Stfsx => "stfsx",
        Stfsux => "stfsux",
        Stfd => "stfd",
        Stfdu => "stfdu",
        Stfdx => "stfdx",
        Stfdux => "stfdux",
        Stfiwx => "stfiwx",
        Fadd => "fadd",
        Fsub => "fsub",
        Fmul => "fmul",
        Fdiv => "fdiv",
        Fsqrt => "fsqrt",
        Fsel => "fsel",
        Fre => "fre",
        Frsqrte => "frsqrte",
        Fmadd => "fmadd",
        Fmsub => "fmsub",
        Fnmadd => "fnmadd",
        Fnmsub => "fnmsub",
        Fadds => "fadds",
        Fsubs => "fsubs",
        Fmuls => "fmuls",
        Fdivs => "fdivs",
        Fsqrts => "fsqrts",
        Fres => "fres",
        Frsqrtes => "frsqrtes",
        Fmadds => "fmadds",
        Fmsubs => "fmsubs",
        Fnmadds => "fnmadds",
        Fnmsubs => "fnmsubs",
        Fmr => "fmr",
        Fneg => "fneg",
        Fabs => "fabs",
        Fnabs => "fnabs",
        Frsp => "frsp",
        Fctiw => "fctiw",
        Fctiwz => "fctiwz",
        Fcmpu => "fcmpu",
        Fcmpo => "fcmpo",
        Mffs => "mffs",
        Mtfsf => "mtfsf",
        Mtfsfi => "mtfsfi",
        Mtfsb0 => "mtfsb0",
        Mtfsb1 => "mtfsb1",
        Mcrfs => "mcrfs",
        Mfspr => "mfspr",
        Mtspr => "mtspr",
        Mftb => "mftb",
        Sync => "sync",
        Eieio => "eieio",
        Isync => "isync",
        Icbi => "icbi",
        Dcbz => "dcbz",
        Dcbf => "dcbf",
        Dcbst => "dcbst",
        Dcbt => "dcbt",
        Dcbtst => "dcbtst",
        Dcbi => "dcbi",
        Mfmsr => "mfmsr",
        Mtmsr => "mtmsr",
        Mfsr => "mfsr",
        Mfsrin => "mfsrin",
        Mtsr => "mtsr",
        Mtsrin => "mtsrin",
        Rfi => "rfi",
        Tlbie => "tlbie",
        Tlbsync => "tlbsync",
    }
}

/// The `tw`/`twi` condition names LLVM prints for a `TO` field.
fn trap_cond(to: u8) -> Option<&'static str> {
    Some(match to {
        16 => "lt",
        4 => "eq",
        8 => "gt",
        24 => "ne",
        2 => "llt",
        1 => "lgt",
        31 => "u",
        _ => return None,
    })
}

/// The branch-prediction suffix of a `BO` whose two low "at" bits are a hint.
fn hint(at: u8) -> Option<&'static str> {
    match at {
        0 => Some(""),
        2 => Some("-"),
        3 => Some("+"),
        _ => None,
    }
}

fn bc_text(i: &Inst, pc: u64) -> String {
    let (bo, bi) = (i.bo(), i.bi());
    let l = if i.lk() { "l" } else { "" };
    let a = if i.aa() { "a" } else { "" };
    let t = i.target(pc);
    // Branch if the CR bit is true/false: BO = 011at / 001at.
    if bo & 0b11100 == 0b01100 || bo & 0b11100 == 0b00100 {
        if let Some(h) = hint(bo & 3) {
            let m = if bo & 0b01000 != 0 { "bt" } else { "bf" };
            return format!("{m}{l}{a}{h} {bi}, {t:#x}");
        }
    }
    // Decrement CTR and branch on it alone: BO = 1a00t / 1a01t, where the
    // "at" hint is BO bits 1 and 4.
    // Decrement CTR and also test a CR bit: BO = 0000y bdnzf, 0001y bdzf,
    // 0100y bdnzt, 0101y bdzt.
    if bo & 0b10100 == 0 && bo & 1 == 0 {
        let m = ["bdnzf", "bdzf", "bdnzt", "bdzt"][(((bo >> 3) & 1) << 1 | ((bo >> 1) & 1)) as usize];
        return format!("{m}{l}{a} {bi}, {t:#x}");
    }
    if bo & 0b10100 == 0b10000 && bi == 0 {
        let at = ((bo >> 3) & 1) << 1 | (bo & 1);
        if let Some(h) = hint(at) {
            let m = if bo & 0b00010 != 0 { "bdz" } else { "bdnz" };
            return format!("{m}{l}{a}{h} {t:#x}");
        }
    }
    format!("bc{l}{a} {bo}, {bi}, {t:#x}")
}

fn bclr_text(i: &Inst) -> String {
    let (bo, bi) = (i.bo(), i.bi());
    let l = if i.lk() { "l" } else { "" };
    let (base, name) = if i.op == Op::Bclr { ("lr", "bclr") } else { ("ctr", "bcctr") };
    if bo == 20 && bi == 0 && i.bh() == 0 {
        return format!("b{base}{l}");
    }
    if i.bh() != 0 {
        return format!("{name}{l} {bo}, {bi}, {}", i.bh());
    }
    format!("{name}{l} {bo}, {bi}")
}

fn rlwinm_text(i: &Inst) -> String {
    let dot = if i.rc() { "." } else { "" };
    let (ra, rs, sh, mb, me) = (i.ra(), i.rt(), i.rb(), i.mb(), i.me());
    // LLVM's printer substitutes slwi/srwi for the non-record form only, and
    // before its aliases; rotlwi and clrlwi are aliases of both forms.
    if !i.rc() && mb == 0 && sh + me == 31 {
        return format!("slwi {ra}, {rs}, {sh}");
    }
    if !i.rc() && me == 31 && sh != 0 && sh + mb == 32 {
        return format!("srwi {ra}, {rs}, {mb}");
    }
    if mb == 0 && me == 31 {
        return format!("rotlwi{dot} {ra}, {rs}, {sh}");
    }
    if sh == 0 && me == 31 {
        return format!("clrlwi{dot} {ra}, {rs}, {mb}");
    }
    format!("rlwinm{dot} {ra}, {rs}, {sh}, {mb}, {me}")
}
