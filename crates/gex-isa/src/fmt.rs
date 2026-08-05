//! Disassembly text for [`Inst`].
//!
//! The output follows binutils syntax closely enough to diff against
//! `objdump -d` over real binaries, which is how the decoder is validated in
//! bulk (`tests/diff-fuzz`). Preferred aliases (`mov`, `cmp`, `lsl`, `ubfx`, …)
//! are reproduced, because objdump prints them and a raw `orr`/`subs`/`ubfm`
//! would show up as a spurious diff.

use crate::*;
use core::fmt::Write;

fn reg(r: RegSel, sz: Sz) -> String {
    let p = if sz.is64() { 'x' } else { 'w' };
    match r {
        RegSel::Gp(n) => format!("{p}{n}"),
        RegSel::Zr => format!("{p}zr"),
        RegSel::Sp => {
            if sz.is64() {
                "sp".into()
            } else {
                "wsp".into()
            }
        }
    }
}

fn shifted(rm: RegSel, sz: Sz, shift: ShiftType, amount: u32) -> String {
    if amount == 0 && shift == ShiftType::Lsl {
        reg(rm, sz)
    } else {
        format!("{}, {} #{amount}", reg(rm, sz), shift.name())
    }
}

fn extended(rm: RegSel, sz: Sz, ext: Extend, amount: u32) -> String {
    // The register printed is 32-bit for byte/half/word extensions.
    let rsz = if ext.src_bits() == 64 { Sz::X } else { Sz::W };
    let mut s = format!("{}, {}", reg(rm, rsz), ext.name());
    if amount != 0 {
        let _ = write!(s, " #{amount}");
    }
    // `uxtx`/`sxtx` with a 64-bit operand prints as `lsl` when it is the
    // default extension, matching binutils.
    if sz.is64() && ext == Extend::Uxtx {
        s = if amount == 0 {
            reg(rm, Sz::X)
        } else {
            format!("{}, lsl #{amount}", reg(rm, Sz::X))
        };
    }
    s
}

/// Data immediates print in hex, matching binutils.
fn imm(v: u64) -> String {
    format!("#0x{v:x}")
}

fn addr_str(a: &AddrMode) -> String {
    match *a {
        AddrMode::Offset { rn, off } => {
            if off == 0 {
                format!("[{}]", reg(rn, Sz::X))
            } else if off < 0 {
                format!("[{}, #-0x{:x}]", reg(rn, Sz::X), -off)
            } else {
                format!("[{}, #0x{:x}]", reg(rn, Sz::X), off)
            }
        }
        AddrMode::PostIndex { rn, off } => {
            if off < 0 {
                format!("[{}], #-0x{:x}", reg(rn, Sz::X), -off)
            } else {
                format!("[{}], #0x{:x}", reg(rn, Sz::X), off)
            }
        }
        AddrMode::PreIndex { rn, off } => {
            if off < 0 {
                format!("[{}, #-0x{:x}]!", reg(rn, Sz::X), -off)
            } else {
                format!("[{}, #0x{:x}]!", reg(rn, Sz::X), off)
            }
        }
        AddrMode::RegOffset {
            rn,
            rm,
            ext,
            amount,
        } => {
            let rsz = if ext.src_bits() == 64 { Sz::X } else { Sz::W };
            let mut s = format!("[{}, {}", reg(rn, Sz::X), reg(rm, rsz));
            if ext == Extend::Uxtx {
                if amount != 0 {
                    let _ = write!(s, ", lsl #{amount}");
                }
            } else {
                let _ = write!(s, ", {}", ext.name());
                if amount != 0 {
                    let _ = write!(s, " #{amount}");
                }
            }
            s.push(']');
            s
        }
        AddrMode::Literal { addr } => format!("0x{addr:x}"),
    }
}

/// Render `inst` as assembly text. `pc` is unused for absolute operands (they
/// were resolved at decode time) but kept for symmetry with [`decode`].
pub fn disasm(inst: &Inst, _pc: u64) -> String {
    match *inst {
        Inst::Unknown { raw } => format!(".inst 0x{raw:08x} ; undecoded"),
        Inst::Undefined { raw } => format!("udf ; 0x{raw:08x}"),
        // Hint space. The pointer-authentication hints (`paciasp`, `autiasp`,
        // …) are genuine no-ops on a PAC-less implementation: signing and
        // authenticating are both the identity, so they stay consistent.
        Inst::Hint { imm: h } => match h {
            0 => "nop".into(),
            1 => "yield".into(),
            2 => "wfe".into(),
            3 => "wfi".into(),
            4 => "sev".into(),
            5 => "sevl".into(),
            6 => "dgh".into(),
            7 => "xpaclri".into(),
            8 => "pacia1716".into(),
            10 => "pacib1716".into(),
            12 => "autia1716".into(),
            14 => "autib1716".into(),
            16 => "esb".into(),
            17 => "psb".into(),
            18 => "tsb".into(),
            20 => "csdb".into(),
            24 => "paciaz".into(),
            25 => "paciasp".into(),
            26 => "pacibz".into(),
            27 => "pacibsp".into(),
            28 => "autiaz".into(),
            29 => "autiasp".into(),
            30 => "autibz".into(),
            31 => "autibsp".into(),
            32 => "bti".into(),
            34 => "bti c".into(),
            36 => "bti j".into(),
            38 => "bti jc".into(),
            _ => format!("hint #{h}"),
        },
        Inst::Clrex => "clrex".into(),

        Inst::Adr { rd, target } => format!("adr {}, 0x{target:x}", reg(rd, Sz::X)),
        Inst::Adrp { rd, target } => format!("adrp {}, 0x{target:x}", reg(rd, Sz::X)),

        Inst::AddSubImm {
            sz,
            op,
            set_flags,
            rd,
            rn,
            imm: v,
        } => {
            let m = mnem_addsub(op, set_flags);
            // cmp/cmn alias: flags set and destination is the zero register.
            if set_flags && rd == RegSel::Zr {
                let alias = if op == AddOp::Sub { "cmp" } else { "cmn" };
                return format!("{alias} {}, {}", reg(rn, sz), imm(v));
            }
            // mov to/from sp: `add rd, rn, #0` with an SP operand.
            if v == 0 && !set_flags && op == AddOp::Add && (rd == RegSel::Sp || rn == RegSel::Sp) {
                return format!("mov {}, {}", reg(rd, sz), reg(rn, sz));
            }
            format!("{m} {}, {}, {}", reg(rd, sz), reg(rn, sz), imm(v))
        }
        Inst::LogicalImm {
            sz,
            op,
            set_flags,
            rd,
            rn,
            imm: v,
        } => {
            if set_flags && rd == RegSel::Zr {
                return format!("tst {}, {}", reg(rn, sz), imm(v));
            }
            if op == LogOp::Orr && rn == RegSel::Zr && !set_flags {
                return format!("mov {}, {}", reg(rd, sz), imm(v));
            }
            let m = match (op, set_flags) {
                (LogOp::And, false) => "and",
                (LogOp::And, true) => "ands",
                (LogOp::Orr, _) => "orr",
                (LogOp::Eor, _) => "eor",
            };
            format!("{m} {}, {}, {}", reg(rd, sz), reg(rn, sz), imm(v))
        }
        Inst::MoveWide {
            sz,
            op,
            rd,
            imm16,
            shift,
        } => {
            // `mov (wide immediate)` is the preferred alias for movz/movn
            // unless the immediate is zero with a non-zero shift.
            if op != MoveOp::Movk && !(imm16 == 0 && shift != 0) {
                let v = imm16 << shift;
                let v = if op == MoveOp::Movn { !v } else { v } & sz.mask();
                return format!("mov {}, #0x{v:x}", reg(rd, sz));
            }
            let m = match op {
                MoveOp::Movz => "movz",
                MoveOp::Movn => "movn",
                MoveOp::Movk => "movk",
            };
            if shift == 0 {
                format!("{m} {}, {}", reg(rd, sz), imm(imm16))
            } else {
                format!("{m} {}, {}, lsl #{shift}", reg(rd, sz), imm(imm16))
            }
        }
        Inst::Bitfield {
            sz,
            op,
            rd,
            rn,
            immr,
            imms,
        } => fmt_bitfield(sz, op, rd, rn, immr, imms),
        Inst::Extract {
            sz,
            rd,
            rn,
            rm,
            lsb,
        } => {
            if rn == rm {
                format!("ror {}, {}, #{lsb}", reg(rd, sz), reg(rn, sz))
            } else {
                format!(
                    "extr {}, {}, {}, #{lsb}",
                    reg(rd, sz),
                    reg(rn, sz),
                    reg(rm, sz)
                )
            }
        }

        Inst::AddSubShifted {
            sz,
            op,
            set_flags,
            rd,
            rn,
            rm,
            shift,
            amount,
        } => {
            if set_flags && rd == RegSel::Zr {
                let alias = if op == AddOp::Sub { "cmp" } else { "cmn" };
                return format!("{alias} {}, {}", reg(rn, sz), shifted(rm, sz, shift, amount));
            }
            if op == AddOp::Sub && rn == RegSel::Zr {
                let alias = if set_flags { "negs" } else { "neg" };
                return format!("{alias} {}, {}", reg(rd, sz), shifted(rm, sz, shift, amount));
            }
            format!(
                "{} {}, {}, {}",
                mnem_addsub(op, set_flags),
                reg(rd, sz),
                reg(rn, sz),
                shifted(rm, sz, shift, amount)
            )
        }
        Inst::AddSubExtended {
            sz,
            op,
            set_flags,
            rd,
            rn,
            rm,
            ext,
            amount,
        } => {
            if set_flags && rd == RegSel::Zr {
                let alias = if op == AddOp::Sub { "cmp" } else { "cmn" };
                return format!(
                    "{alias} {}, {}",
                    reg(rn, sz),
                    extended(rm, sz, ext, amount)
                );
            }
            format!(
                "{} {}, {}, {}",
                mnem_addsub(op, set_flags),
                reg(rd, sz),
                reg(rn, sz),
                extended(rm, sz, ext, amount)
            )
        }
        Inst::AddSubCarry {
            sz,
            op,
            set_flags,
            rd,
            rn,
            rm,
        } => {
            // ngc/ngcs: sbc/sbcs with the zero register as first source.
            if op == AddOp::Sub && rn == RegSel::Zr {
                let alias = if set_flags { "ngcs" } else { "ngc" };
                return format!("{alias} {}, {}", reg(rd, sz), reg(rm, sz));
            }
            let m = match (op, set_flags) {
                (AddOp::Add, false) => "adc",
                (AddOp::Add, true) => "adcs",
                (AddOp::Sub, false) => "sbc",
                (AddOp::Sub, true) => "sbcs",
            };
            format!("{m} {}, {}, {}", reg(rd, sz), reg(rn, sz), reg(rm, sz))
        }
        Inst::LogicalShifted {
            sz,
            op,
            set_flags,
            invert,
            rd,
            rn,
            rm,
            shift,
            amount,
        } => {
            // mov (register) = orr rd, xzr, rm
            if op == LogOp::Orr
                && !invert
                && !set_flags
                && rn == RegSel::Zr
                && amount == 0
                && shift == ShiftType::Lsl
            {
                return format!("mov {}, {}", reg(rd, sz), reg(rm, sz));
            }
            // mvn = orn rd, xzr, rm
            if op == LogOp::Orr && invert && !set_flags && rn == RegSel::Zr {
                return format!("mvn {}, {}", reg(rd, sz), shifted(rm, sz, shift, amount));
            }
            if op == LogOp::And && set_flags && rd == RegSel::Zr && !invert {
                return format!("tst {}, {}", reg(rn, sz), shifted(rm, sz, shift, amount));
            }
            let m = match (op, invert, set_flags) {
                (LogOp::And, false, false) => "and",
                (LogOp::And, false, true) => "ands",
                (LogOp::And, true, false) => "bic",
                (LogOp::And, true, true) => "bics",
                (LogOp::Orr, false, _) => "orr",
                (LogOp::Orr, true, _) => "orn",
                (LogOp::Eor, false, _) => "eor",
                (LogOp::Eor, true, _) => "eon",
            };
            format!(
                "{m} {}, {}, {}",
                reg(rd, sz),
                reg(rn, sz),
                shifted(rm, sz, shift, amount)
            )
        }
        Inst::CondSelect {
            sz,
            rd,
            rn,
            rm,
            cond,
            else_inc,
            else_inv,
        } => {
            // cset/csetm: rn == rm == zr, condition inverted.
            if rn == RegSel::Zr && rm == RegSel::Zr && !cond.is_always() {
                if else_inc && !else_inv {
                    return format!("cset {}, {}", reg(rd, sz), cond.invert().name());
                }
                if else_inv && !else_inc {
                    return format!("csetm {}, {}", reg(rd, sz), cond.invert().name());
                }
            }
            // cinc/cinv/cneg: rn == rm, condition inverted.
            if rn == rm && rn != RegSel::Zr && !cond.is_always() {
                let alias = match (else_inv, else_inc) {
                    (false, true) => Some("cinc"),
                    (true, false) => Some("cinv"),
                    (true, true) => Some("cneg"),
                    (false, false) => None,
                };
                if let Some(alias) = alias {
                    return format!(
                        "{alias} {}, {}, {}",
                        reg(rd, sz),
                        reg(rn, sz),
                        cond.invert().name()
                    );
                }
            }
            let m = match (else_inv, else_inc) {
                (false, false) => "csel",
                (false, true) => "csinc",
                (true, false) => "csinv",
                (true, true) => "csneg",
            };
            format!(
                "{m} {}, {}, {}, {}",
                reg(rd, sz),
                reg(rn, sz),
                reg(rm, sz),
                cond.name()
            )
        }
        Inst::CondCompare {
            sz,
            op,
            rn,
            rm,
            imm: v,
            nzcv,
            cond,
        } => {
            let m = if op == AddOp::Sub { "ccmp" } else { "ccmn" };
            let second = match rm {
                Some(r) => reg(r, sz),
                None => format!("#{v}"),
            };
            format!(
                "{m} {}, {second}, #{nzcv}, {}",
                reg(rn, sz),
                cond.name()
            )
        }
        Inst::DataProc1 { sz, op, rd, rn } => {
            let m = match op {
                Dp1Op::Rbit => "rbit",
                Dp1Op::Rev16 => "rev16",
                Dp1Op::Rev32 => "rev32",
                Dp1Op::Rev => "rev",
                Dp1Op::Clz => "clz",
                Dp1Op::Cls => "cls",
            };
            format!("{m} {}, {}", reg(rd, sz), reg(rn, sz))
        }
        Inst::DataProc2 { sz, op, rd, rn, rm } => {
            let m = match op {
                Dp2Op::Udiv => "udiv",
                Dp2Op::Sdiv => "sdiv",
                Dp2Op::Lslv => "lsl",
                Dp2Op::Lsrv => "lsr",
                Dp2Op::Asrv => "asr",
                Dp2Op::Rorv => "ror",
                Dp2Op::Crc32b => "crc32b",
                Dp2Op::Crc32h => "crc32h",
                Dp2Op::Crc32w => "crc32w",
                Dp2Op::Crc32x => "crc32x",
                Dp2Op::Crc32cb => "crc32cb",
                Dp2Op::Crc32ch => "crc32ch",
                Dp2Op::Crc32cw => "crc32cw",
                Dp2Op::Crc32cx => "crc32cx",
            };
            format!("{m} {}, {}, {}", reg(rd, sz), reg(rn, sz), reg(rm, sz))
        }
        Inst::DataProc3 {
            sz,
            op,
            rd,
            rn,
            rm,
            ra,
        } => {
            let (m, wide) = match op {
                Dp3Op::Madd => ("madd", false),
                Dp3Op::Msub => ("msub", false),
                Dp3Op::Smaddl => ("smaddl", true),
                Dp3Op::Smsubl => ("smsubl", true),
                Dp3Op::Umaddl => ("umaddl", true),
                Dp3Op::Umsubl => ("umsubl", true),
                Dp3Op::Smulh => ("smulh", false),
                Dp3Op::Umulh => ("umulh", false),
            };
            let ssz = if wide { Sz::W } else { sz };
            if matches!(op, Dp3Op::Smulh | Dp3Op::Umulh) {
                return format!("{m} {}, {}, {}", reg(rd, sz), reg(rn, sz), reg(rm, sz));
            }
            if ra == RegSel::Zr {
                let alias = match op {
                    Dp3Op::Madd => "mul",
                    Dp3Op::Msub => "mneg",
                    Dp3Op::Smaddl => "smull",
                    Dp3Op::Smsubl => "smnegl",
                    Dp3Op::Umaddl => "umull",
                    Dp3Op::Umsubl => "umnegl",
                    _ => unreachable!(),
                };
                return format!("{alias} {}, {}, {}", reg(rd, sz), reg(rn, ssz), reg(rm, ssz));
            }
            format!(
                "{m} {}, {}, {}, {}",
                reg(rd, sz),
                reg(rn, ssz),
                reg(rm, ssz),
                reg(ra, sz)
            )
        }

        Inst::Branch { target, link } => {
            format!("{} 0x{target:x}", if link { "bl" } else { "b" })
        }
        Inst::BranchCond { cond, target } => format!("b.{} 0x{target:x}", cond.name()),
        Inst::CompareBranch {
            sz,
            rt,
            target,
            nonzero,
        } => format!(
            "{} {}, 0x{target:x}",
            if nonzero { "cbnz" } else { "cbz" },
            reg(rt, sz)
        ),
        Inst::TestBranch {
            rt,
            bit,
            target,
            nonzero,
        } => format!(
            "{} {}, #{bit}, 0x{target:x}",
            if nonzero { "tbnz" } else { "tbz" },
            reg(rt, if bit >= 32 { Sz::X } else { Sz::W })
        ),
        Inst::BranchReg { rn, link, is_ret } => {
            if is_ret {
                if rn == RegSel::Gp(30) {
                    "ret".into()
                } else {
                    format!("ret {}", reg(rn, Sz::X))
                }
            } else if link {
                format!("blr {}", reg(rn, Sz::X))
            } else {
                format!("br {}", reg(rn, Sz::X))
            }
        }

        Inst::Svc { imm: v } => format!("svc #0x{v:x}"),
        Inst::Brk { imm: v } => format!("brk #0x{v:x}"),

        Inst::SysRegRead { rt, sysreg } => {
            format!("mrs {}, {}", reg(rt, Sz::X), sysreg_name(sysreg))
        }
        Inst::SysRegWrite { rt, sysreg } => {
            format!("msr {}, {}", sysreg_name(sysreg), reg(rt, Sz::X))
        }
        Inst::SysMaintenance { op, rt } => {
            let m = match op {
                SysMaintOp::DcZva => "dc zva",
                SysMaintOp::DcClean => "dc",
                SysMaintOp::IcInvalidate => "ic",
                SysMaintOp::Other => "sys",
            };
            format!("{m} {}", reg(rt, Sz::X))
        }
        Inst::Barrier { kind, option } => {
            let m = match kind {
                BarrierKind::Dmb => "dmb",
                BarrierKind::Dsb => "dsb",
                BarrierKind::Isb => "isb",
                BarrierKind::Sb => "sb",
            };
            if kind == BarrierKind::Isb {
                return "isb".into();
            }
            let o = match option {
                0b1111 => "sy",
                0b1110 => "st",
                0b1101 => "ld",
                0b1011 => "ish",
                0b1010 => "ishst",
                0b1001 => "ishld",
                0b0111 => "nsh",
                0b0110 => "nshst",
                0b0011 => "osh",
                _ => "sy",
            };
            format!("{m} {o}")
        }

        Inst::Load {
            sz,
            dest,
            signed,
            rt,
            ref addr,
            order,
        } => {
            let m = load_mnemonic(sz, dest, signed, order, addr);
            format!("{m} {}, {}", reg(rt, dest), addr_str(addr))
        }
        Inst::Store {
            sz,
            rt,
            ref addr,
            order,
        } => {
            let base = if order == Ordering::Release {
                match sz {
                    MemSz::B => "stlrb",
                    MemSz::H => "stlrh",
                    _ => "stlr",
                }
            } else {
                match sz {
                    MemSz::B => "strb",
                    MemSz::H => "strh",
                    _ => "str",
                }
            };
            let rsz = if sz == MemSz::D { Sz::X } else { Sz::W };
            format!("{base} {}, {}", reg(rt, rsz), addr_str(addr))
        }
        Inst::LoadPair {
            sz,
            signed,
            rt,
            rt2,
            ref addr,
        } => {
            let rsz = if sz == MemSz::D || signed { Sz::X } else { Sz::W };
            let m = if signed { "ldpsw" } else { "ldp" };
            format!("{m} {}, {}, {}", reg(rt, rsz), reg(rt2, rsz), addr_str(addr))
        }
        Inst::StorePair {
            sz,
            rt,
            rt2,
            ref addr,
        } => {
            let rsz = if sz == MemSz::D { Sz::X } else { Sz::W };
            format!("stp {}, {}, {}", reg(rt, rsz), reg(rt2, rsz), addr_str(addr))
        }
        Inst::Prefetch { ref addr } => format!("prfm {}", addr_str(addr)),

        Inst::LoadExclusive { sz, rt, rn, order } => {
            let acq = order == Ordering::Acquire;
            let m = match (sz, acq) {
                (MemSz::B, false) => "ldxrb",
                (MemSz::B, true) => "ldaxrb",
                (MemSz::H, false) => "ldxrh",
                (MemSz::H, true) => "ldaxrh",
                (_, false) => "ldxr",
                (_, true) => "ldaxr",
            };
            let rsz = if sz == MemSz::D { Sz::X } else { Sz::W };
            format!("{m} {}, [{}]", reg(rt, rsz), reg(rn, Sz::X))
        }
        Inst::StoreExclusive {
            sz,
            rs,
            rt,
            rn,
            order,
        } => {
            let rel = order == Ordering::Release;
            let m = match (sz, rel) {
                (MemSz::B, false) => "stxrb",
                (MemSz::B, true) => "stlxrb",
                (MemSz::H, false) => "stxrh",
                (MemSz::H, true) => "stlxrh",
                (_, false) => "stxr",
                (_, true) => "stlxr",
            };
            let rsz = if sz == MemSz::D { Sz::X } else { Sz::W };
            format!(
                "{m} {}, {}, [{}]",
                reg(rs, Sz::W),
                reg(rt, rsz),
                reg(rn, Sz::X)
            )
        }
        Inst::CompareSwap {
            sz,
            rs,
            rt,
            rn,
            acquire,
            release,
        } => {
            let suffix = match (acquire, release) {
                (false, false) => "",
                (true, false) => "a",
                (false, true) => "l",
                (true, true) => "al",
            };
            let w = match sz {
                MemSz::B => "b",
                MemSz::H => "h",
                _ => "",
            };
            let rsz = if sz == MemSz::D { Sz::X } else { Sz::W };
            format!(
                "cas{suffix}{w} {}, {}, [{}]",
                reg(rs, rsz),
                reg(rt, rsz),
                reg(rn, Sz::X)
            )
        }
        Inst::AtomicRmw {
            sz,
            op,
            rs,
            rt,
            rn,
            acquire,
            release,
        } => {
            let base = match op {
                RmwOp::Add => "ldadd",
                RmwOp::Clr => "ldclr",
                RmwOp::Eor => "ldeor",
                RmwOp::Set => "ldset",
                RmwOp::Smax => "ldsmax",
                RmwOp::Smin => "ldsmin",
                RmwOp::Umax => "ldumax",
                RmwOp::Umin => "ldumin",
                RmwOp::Swp => "swp",
            };
            let suffix = match (acquire, release) {
                (false, false) => "",
                (true, false) => "a",
                (false, true) => "l",
                (true, true) => "al",
            };
            let w = match sz {
                MemSz::B => "b",
                MemSz::H => "h",
                _ => "",
            };
            let rsz = if sz == MemSz::D { Sz::X } else { Sz::W };
            format!(
                "{base}{suffix}{w} {}, {}, [{}]",
                reg(rs, rsz),
                reg(rt, rsz),
                reg(rn, Sz::X)
            )
        }
    }
}

fn load_mnemonic(
    sz: MemSz,
    dest: Sz,
    signed: bool,
    order: Ordering,
    addr: &AddrMode,
) -> &'static str {
    if order == Ordering::Acquire {
        return match sz {
            MemSz::B => "ldarb",
            MemSz::H => "ldarh",
            _ => "ldar",
        };
    }
    if matches!(addr, AddrMode::Literal { .. }) {
        return if signed { "ldrsw" } else { "ldr" };
    }
    match (sz, signed, dest) {
        (MemSz::B, false, _) => "ldrb",
        (MemSz::H, false, _) => "ldrh",
        (MemSz::B, true, _) => "ldrsb",
        (MemSz::H, true, _) => "ldrsh",
        (MemSz::W, true, _) => "ldrsw",
        _ => "ldr",
    }
}

fn mnem_addsub(op: AddOp, set_flags: bool) -> &'static str {
    match (op, set_flags) {
        (AddOp::Add, false) => "add",
        (AddOp::Add, true) => "adds",
        (AddOp::Sub, false) => "sub",
        (AddOp::Sub, true) => "subs",
    }
}

fn fmt_bitfield(sz: Sz, op: BfOp, rd: RegSel, rn: RegSel, immr: u32, imms: u32) -> String {
    let bits = sz.bits();
    let d = reg(rd, sz);
    let n = reg(rn, sz);
    match op {
        BfOp::Ubfm => {
            if imms + 1 == immr {
                return format!("lsl {d}, {n}, #{}", bits - immr);
            }
            if imms == bits - 1 {
                return format!("lsr {d}, {n}, #{immr}");
            }
            if immr == 0 {
                match imms {
                    7 => return format!("uxtb {d}, {}", reg(rn, Sz::W)),
                    15 => return format!("uxth {d}, {}", reg(rn, Sz::W)),
                    _ => {}
                }
            }
            if imms >= immr {
                return format!("ubfx {d}, {n}, #{immr}, #{}", imms - immr + 1);
            }
            format!(
                "ubfiz {d}, {n}, #{}, #{}",
                bits - immr,
                imms + 1
            )
        }
        BfOp::Sbfm => {
            if imms == bits - 1 {
                return format!("asr {d}, {n}, #{immr}");
            }
            if immr == 0 {
                match imms {
                    7 => return format!("sxtb {d}, {}", reg(rn, Sz::W)),
                    15 => return format!("sxth {d}, {}", reg(rn, Sz::W)),
                    31 if sz.is64() => return format!("sxtw {d}, {}", reg(rn, Sz::W)),
                    _ => {}
                }
            }
            if imms >= immr {
                return format!("sbfx {d}, {n}, #{immr}, #{}", imms - immr + 1);
            }
            format!("sbfiz {d}, {n}, #{}, #{}", bits - immr, imms + 1)
        }
        BfOp::Bfm => {
            if imms >= immr {
                if rn == RegSel::Zr {
                    return format!("bfc {d}, #{immr}, #{}", imms - immr + 1);
                }
                return format!("bfxil {d}, {n}, #{immr}, #{}", imms - immr + 1);
            }
            format!("bfi {d}, {n}, #{}, #{}", bits - immr, imms + 1)
        }
    }
}

fn sysreg_name(s: SysReg) -> String {
    match s {
        SysReg::TpidrEl0 => "tpidr_el0".into(),
        SysReg::TpidrroEl0 => "tpidrro_el0".into(),
        SysReg::Cntvct => "cntvct_el0".into(),
        SysReg::Cntfrq => "cntfrq_el0".into(),
        SysReg::Ctr => "ctr_el0".into(),
        SysReg::Dczid => "dczid_el0".into(),
        SysReg::Midr => "midr_el1".into(),
        SysReg::Fpcr => "fpcr".into(),
        SysReg::Fpsr => "fpsr".into(),
        SysReg::Nzcv => "nzcv".into(),
        SysReg::Other(v) => {
            let op0 = (v >> 14) & 3;
            let op1 = (v >> 11) & 7;
            let crn = (v >> 7) & 15;
            let crm = (v >> 3) & 15;
            let op2 = v & 7;
            format!("s{op0}_{op1}_c{crn}_c{crm}_{op2}")
        }
    }
}
