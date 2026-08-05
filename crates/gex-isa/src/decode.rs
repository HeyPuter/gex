//! The A64 decoder.
//!
//! Dispatch follows the encoding-group tree in the Arm ARM (section C4.1):
//! bits `[28:25]` select the top-level group, then each group is discriminated
//! by exact bit tests. Tests are written as explicit mask/value comparisons
//! rather than nested matches so they can be checked line-by-line against the
//! manual.

use crate::*;

/// Extract bits `[hi:lo]` inclusive.
#[inline]
fn bits(raw: u32, hi: u32, lo: u32) -> u32 {
    (raw >> lo) & ((1u32 << (hi - lo + 1)) - 1)
}

/// Extract bit `n`.
#[inline]
fn bit(raw: u32, n: u32) -> u32 {
    (raw >> n) & 1
}

/// Sign-extend the low `n` bits of `v`.
#[inline]
fn sext(v: u32, n: u32) -> i64 {
    let shift = 64 - n;
    ((v as u64) << shift) as i64 >> shift
}

/// Decode one 32-bit A64 instruction located at guest address `pc`.
///
/// `pc` is needed because PC-relative operands (branches, `adr`, literal
/// loads) are resolved to absolute guest addresses here.
pub fn decode(raw: u32, pc: u64) -> Inst {
    match bits(raw, 28, 25) {
        0b0000 => decode_reserved(raw),
        0b0001 | 0b0011 => Inst::Unknown { raw },
        // SVE / SME — not in scope; classified so the error is precise.
        0b0010 => Inst::Unknown { raw },
        0b1000 | 0b1001 => decode_dp_imm(raw, pc),
        0b1010 | 0b1011 => decode_branch_sys(raw, pc),
        0b0100 | 0b0110 | 0b1100 | 0b1110 => decode_load_store(raw, pc),
        0b0101 | 0b1101 => decode_dp_reg(raw),
        0b0111 | 0b1111 => decode_simd_fp(raw),
        _ => Inst::Unknown { raw },
    }
}

fn decode_reserved(raw: u32) -> Inst {
    // UDF: 0000 0000 0000 0000 iiii iiii iiii iiii
    if bits(raw, 31, 16) == 0 {
        Inst::Undefined { raw }
    } else {
        Inst::Unknown { raw }
    }
}

// ---------------------------------------------------------------------------
// Data processing — immediate
// ---------------------------------------------------------------------------

fn decode_dp_imm(raw: u32, pc: u64) -> Inst {
    match bits(raw, 25, 23) {
        0b000 | 0b001 => {
            // PC-relative addressing.
            let rd = RegSel::gp(bits(raw, 4, 0));
            let immlo = bits(raw, 30, 29);
            let immhi = bits(raw, 23, 5);
            let imm = sext((immhi << 2) | immlo, 21);
            if bit(raw, 31) == 0 {
                Inst::Adr {
                    rd,
                    target: pc.wrapping_add(imm as u64),
                }
            } else {
                Inst::Adrp {
                    rd,
                    target: (pc & !0xfff).wrapping_add((imm << 12) as u64),
                }
            }
        }
        0b010 => {
            // Add/subtract (immediate).
            let sz = Sz::from_sf(bit(raw, 31));
            let op = if bit(raw, 30) == 0 { AddOp::Add } else { AddOp::Sub };
            let set_flags = bit(raw, 29) == 1;
            let sh = bit(raw, 22);
            if bit(raw, 23) == 1 {
                // sh field is 2 bits in the reserved encoding — unallocated.
                return Inst::Undefined { raw };
            }
            let imm12 = bits(raw, 21, 10) as u64;
            let imm = if sh == 1 { imm12 << 12 } else { imm12 };
            // Rd is SP-form only when flags are not set (ADD/SUB vs ADDS/SUBS).
            let rd = if set_flags {
                RegSel::gp(bits(raw, 4, 0))
            } else {
                RegSel::sp(bits(raw, 4, 0))
            };
            Inst::AddSubImm {
                sz,
                op,
                set_flags,
                rd,
                rn: RegSel::sp(bits(raw, 9, 5)),
                imm,
            }
        }
        // Add/subtract (immediate, with tags) — MTE, out of scope.
        0b011 => Inst::Unknown { raw },
        0b100 => {
            // Logical (immediate).
            let sf = bit(raw, 31);
            let sz = Sz::from_sf(sf);
            let n = bit(raw, 22);
            let immr = bits(raw, 21, 16);
            let imms = bits(raw, 15, 10);
            let Some(imm) = decode_bit_masks(n, imms, immr, sf == 1) else {
                return Inst::Undefined { raw };
            };
            let opc = bits(raw, 30, 29);
            let (op, set_flags) = match opc {
                0b00 => (LogOp::And, false),
                0b01 => (LogOp::Orr, false),
                0b10 => (LogOp::Eor, false),
                _ => (LogOp::And, true),
            };
            // ANDS writes a ZR-form Rd; the others write SP-form.
            let rd = if set_flags {
                RegSel::gp(bits(raw, 4, 0))
            } else {
                RegSel::sp(bits(raw, 4, 0))
            };
            Inst::LogicalImm {
                sz,
                op,
                set_flags,
                rd,
                rn: RegSel::gp(bits(raw, 9, 5)),
                imm,
            }
        }
        0b101 => {
            // Move wide (immediate).
            let sf = bit(raw, 31);
            let sz = Sz::from_sf(sf);
            let hw = bits(raw, 22, 21);
            if sf == 0 && hw > 1 {
                return Inst::Undefined { raw };
            }
            let op = match bits(raw, 30, 29) {
                0b00 => MoveOp::Movn,
                0b10 => MoveOp::Movz,
                0b11 => MoveOp::Movk,
                _ => return Inst::Undefined { raw },
            };
            Inst::MoveWide {
                sz,
                op,
                rd: RegSel::gp(bits(raw, 4, 0)),
                imm16: bits(raw, 20, 5) as u64,
                shift: hw * 16,
            }
        }
        0b110 => {
            // Bitfield.
            let sf = bit(raw, 31);
            let sz = Sz::from_sf(sf);
            let n = bit(raw, 22);
            if n != sf {
                return Inst::Undefined { raw };
            }
            let op = match bits(raw, 30, 29) {
                0b00 => BfOp::Sbfm,
                0b01 => BfOp::Bfm,
                0b10 => BfOp::Ubfm,
                _ => return Inst::Undefined { raw },
            };
            let immr = bits(raw, 21, 16);
            let imms = bits(raw, 15, 10);
            if sf == 0 && (immr > 31 || imms > 31) {
                return Inst::Undefined { raw };
            }
            Inst::Bitfield {
                sz,
                op,
                rd: RegSel::gp(bits(raw, 4, 0)),
                rn: RegSel::gp(bits(raw, 9, 5)),
                immr,
                imms,
            }
        }
        _ => {
            // Extract (EXTR).
            let sf = bit(raw, 31);
            let sz = Sz::from_sf(sf);
            if bits(raw, 30, 29) != 0 || bit(raw, 21) != 0 || bit(raw, 22) != sf {
                return Inst::Undefined { raw };
            }
            let imms = bits(raw, 15, 10);
            if sf == 0 && imms > 31 {
                return Inst::Undefined { raw };
            }
            Inst::Extract {
                sz,
                rd: RegSel::gp(bits(raw, 4, 0)),
                rn: RegSel::gp(bits(raw, 9, 5)),
                rm: RegSel::gp(bits(raw, 20, 16)),
                lsb: imms,
            }
        }
    }
}

/// `DecodeBitMasks` from the Arm ARM, immediate form (`immediate = TRUE`).
///
/// Returns `None` for the encodings the architecture leaves UNDEFINED.
pub fn decode_bit_masks(n: u32, imms: u32, immr: u32, sf: bool) -> Option<u64> {
    // len = HighestSetBit(N:NOT(imms)) over 7 bits.
    let combined = ((n & 1) << 6) | ((!imms) & 0x3f);
    if combined == 0 {
        return None;
    }
    let len = 31 - combined.leading_zeros();
    if len < 1 {
        return None;
    }
    let esize = 1u32 << len;
    if !sf && esize > 32 {
        return None;
    }
    let levels = esize - 1;
    let s = imms & levels;
    let r = immr & levels;
    // `imms` all-ones within the element is reserved for the immediate form.
    if s == levels {
        return None;
    }
    let elem_mask = if esize >= 64 {
        u64::MAX
    } else {
        (1u64 << esize) - 1
    };
    let welem = if s + 1 >= 64 {
        u64::MAX
    } else {
        (1u64 << (s + 1)) - 1
    };
    let rotated = if r == 0 {
        welem
    } else {
        ((welem >> r) | (welem << (esize - r))) & elem_mask
    };
    let mut out = 0u64;
    let mut i = 0;
    while i < 64 {
        out |= rotated << i;
        i += esize;
    }
    Some(if sf { out } else { out & 0xffff_ffff })
}

// ---------------------------------------------------------------------------
// Branches, exception generation, system
// ---------------------------------------------------------------------------

fn decode_branch_sys(raw: u32, pc: u64) -> Inst {
    // Unconditional branch (immediate): x00101 imm26
    if bits(raw, 30, 26) == 0b00101 {
        let off = sext(bits(raw, 25, 0), 26) << 2;
        return Inst::Branch {
            target: pc.wrapping_add(off as u64),
            link: bit(raw, 31) == 1,
        };
    }
    // Compare and branch: sf 011010 op imm19 Rt
    if bits(raw, 30, 25) == 0b011010 {
        let off = sext(bits(raw, 23, 5), 19) << 2;
        return Inst::CompareBranch {
            sz: Sz::from_sf(bit(raw, 31)),
            rt: RegSel::gp(bits(raw, 4, 0)),
            target: pc.wrapping_add(off as u64),
            nonzero: bit(raw, 24) == 1,
        };
    }
    // Test and branch: b5 011011 op b40 imm14 Rt
    if bits(raw, 30, 25) == 0b011011 {
        let off = sext(bits(raw, 18, 5), 14) << 2;
        return Inst::TestBranch {
            rt: RegSel::gp(bits(raw, 4, 0)),
            bit: (bit(raw, 31) << 5) | bits(raw, 23, 19),
            target: pc.wrapping_add(off as u64),
            nonzero: bit(raw, 24) == 1,
        };
    }
    // Conditional branch (immediate): 0101010 o1 imm19 o0 cond
    if bits(raw, 31, 25) == 0b0101010 {
        if bit(raw, 24) != 0 || bit(raw, 4) != 0 {
            // o1=1 is unallocated; o0=1 is BC.cond (FEAT_HBC), not supported.
            return Inst::Unknown { raw };
        }
        let off = sext(bits(raw, 23, 5), 19) << 2;
        return Inst::BranchCond {
            cond: Cond(bits(raw, 3, 0) as u8),
            target: pc.wrapping_add(off as u64),
        };
    }
    // Exception generation: 11010100 opc imm16 op2 LL
    if bits(raw, 31, 24) == 0b1101_0100 {
        let opc = bits(raw, 23, 21);
        let imm16 = bits(raw, 20, 5);
        let ll = bits(raw, 1, 0);
        return match (opc, bits(raw, 4, 2), ll) {
            (0b000, 0, 0b01) => Inst::Svc { imm: imm16 },
            (0b001, 0, 0b00) => Inst::Brk { imm: imm16 },
            // HLT, HVC, SMC, DCPS* — not reachable from EL0 userspace.
            _ => Inst::Unknown { raw },
        };
    }
    // Unconditional branch (register): 1101011 opc op2 op3 Rn op4
    if bits(raw, 31, 25) == 0b1101011 {
        let opc = bits(raw, 24, 21);
        let op2 = bits(raw, 20, 16);
        let op3 = bits(raw, 15, 10);
        let rn = bits(raw, 9, 5);
        let op4 = bits(raw, 4, 0);
        if op2 != 0b11111 {
            return Inst::Unknown { raw };
        }
        // op3 != 0 selects the pointer-authentication variants (BRAA etc).
        if op3 != 0 {
            return Inst::Unknown { raw };
        }
        return match (opc, op4) {
            (0b0000, 0) => Inst::BranchReg {
                rn: RegSel::gp(rn),
                link: false,
                is_ret: false,
            },
            (0b0001, 0) => Inst::BranchReg {
                rn: RegSel::gp(rn),
                link: true,
                is_ret: false,
            },
            (0b0010, 0) => Inst::BranchReg {
                rn: RegSel::gp(rn),
                link: false,
                is_ret: true,
            },
            // ERET, DRPS — EL1+.
            _ => Inst::Unknown { raw },
        };
    }
    // System: 1101 0101 00 ...
    if bits(raw, 31, 22) == 0b1101_0101_00 {
        return decode_system(raw);
    }
    Inst::Unknown { raw }
}

fn decode_system(raw: u32) -> Inst {
    let l = bit(raw, 21);
    let op0 = bits(raw, 20, 19);
    let op1 = bits(raw, 18, 16);
    let crn = bits(raw, 15, 12);
    let crm = bits(raw, 11, 8);
    let op2 = bits(raw, 7, 5);
    let rt = bits(raw, 4, 0);

    if l == 0 && op0 == 0b00 {
        // Hints, barriers and PSTATE writes.
        if rt == 0b11111 {
            match (op1, crn) {
                (0b011, 0b0010) => return Inst::Hint { imm: crm * 8 + op2 },
                (0b011, 0b0011) => {
                    return match op2 {
                        0b010 => Inst::Clrex,
                        0b100 => Inst::Barrier {
                            kind: BarrierKind::Dsb,
                            option: crm,
                        },
                        0b101 => Inst::Barrier {
                            kind: BarrierKind::Dmb,
                            option: crm,
                        },
                        0b110 => Inst::Barrier {
                            kind: BarrierKind::Isb,
                            option: crm,
                        },
                        0b111 => Inst::Barrier {
                            kind: BarrierKind::Sb,
                            option: crm,
                        },
                        _ => Inst::Unknown { raw },
                    };
                }
                // MSR (immediate) — PSTATE field writes (DAIFSet, SSBS, …).
                // None are observable to a correct EL0 program under gex.
                (_, 0b0100) => return Inst::Hint { imm: 0 },
                _ => return Inst::Unknown { raw },
            }
        }
        return Inst::Unknown { raw };
    }

    if op0 == 0b01 {
        // SYS / SYSL — cache and TLB maintenance.
        let op = match (op1, crn, crm, op2) {
            (0b011, 0b0111, 0b0100, 0b001) => SysMaintOp::DcZva,
            (_, 0b0111, 0b0101, _) => SysMaintOp::IcInvalidate,
            (_, 0b0111, _, _) => SysMaintOp::DcClean,
            _ => SysMaintOp::Other,
        };
        return Inst::SysMaintenance {
            op,
            rt: RegSel::gp(rt),
        };
    }

    if op0 & 0b10 != 0 {
        // MRS / MSR (register). Architectural op0 is 2 + the encoded value.
        let real_op0 = 2 + (op0 & 1);
        let sysreg = classify_sysreg(real_op0, op1, crn, crm, op2);
        return if l == 1 {
            Inst::SysRegRead {
                rt: RegSel::gp(rt),
                sysreg,
            }
        } else {
            Inst::SysRegWrite {
                rt: RegSel::gp(rt),
                sysreg,
            }
        };
    }

    Inst::Unknown { raw }
}

fn classify_sysreg(op0: u32, op1: u32, crn: u32, crm: u32, op2: u32) -> SysReg {
    match (op0, op1, crn, crm, op2) {
        (3, 3, 13, 0, 2) => SysReg::TpidrEl0,
        (3, 3, 13, 0, 3) => SysReg::TpidrroEl0,
        (3, 3, 14, 0, 0) => SysReg::Cntfrq,
        (3, 3, 14, 0, 2) => SysReg::Cntvct,
        (3, 3, 0, 0, 1) => SysReg::Ctr,
        (3, 3, 0, 0, 7) => SysReg::Dczid,
        (3, 0, 0, 0, 0) => SysReg::Midr,
        (3, 3, 4, 4, 0) => SysReg::Fpcr,
        (3, 3, 4, 4, 1) => SysReg::Fpsr,
        (3, 3, 4, 2, 0) => SysReg::Nzcv,
        _ => SysReg::Other(
            ((op0 as u16) << 14) | ((op1 as u16) << 11) | ((crn as u16) << 7)
                | ((crm as u16) << 3)
                | op2 as u16,
        ),
    }
}

// ---------------------------------------------------------------------------
// Data processing — register
// ---------------------------------------------------------------------------

fn decode_dp_reg(raw: u32) -> Inst {
    let sf = bit(raw, 31);
    let sz = Sz::from_sf(sf);
    let rd = bits(raw, 4, 0);
    let rn = bits(raw, 9, 5);
    let rm = bits(raw, 20, 16);

    // Logical (shifted register): sf opc 01010 shift N Rm imm6 Rn Rd
    if bits(raw, 28, 24) == 0b01010 {
        let amount = bits(raw, 15, 10);
        if sf == 0 && amount > 31 {
            return Inst::Undefined { raw };
        }
        let opc = bits(raw, 30, 29);
        let (op, set_flags) = match opc {
            0b00 => (LogOp::And, false),
            0b01 => (LogOp::Orr, false),
            0b10 => (LogOp::Eor, false),
            _ => (LogOp::And, true),
        };
        return Inst::LogicalShifted {
            sz,
            op,
            set_flags,
            invert: bit(raw, 21) == 1,
            rd: RegSel::gp(rd),
            rn: RegSel::gp(rn),
            rm: RegSel::gp(rm),
            shift: ShiftType::from_bits(bits(raw, 23, 22)),
            amount,
        };
    }

    // Add/subtract (shifted or extended register): sf op S 01011 ...
    if bits(raw, 28, 24) == 0b01011 {
        let op = if bit(raw, 30) == 0 { AddOp::Add } else { AddOp::Sub };
        let set_flags = bit(raw, 29) == 1;
        if bit(raw, 21) == 0 {
            let shift = bits(raw, 23, 22);
            if shift == 0b11 {
                return Inst::Undefined { raw };
            }
            let amount = bits(raw, 15, 10);
            if sf == 0 && amount > 31 {
                return Inst::Undefined { raw };
            }
            return Inst::AddSubShifted {
                sz,
                op,
                set_flags,
                rd: RegSel::gp(rd),
                rn: RegSel::gp(rn),
                rm: RegSel::gp(rm),
                shift: ShiftType::from_bits(shift),
                amount,
            };
        }
        if bits(raw, 23, 22) != 0 {
            return Inst::Undefined { raw };
        }
        let amount = bits(raw, 12, 10);
        if amount > 4 {
            return Inst::Undefined { raw };
        }
        // Extended-register form: Rn is always SP-form; Rd is SP-form unless
        // flags are set.
        return Inst::AddSubExtended {
            sz,
            op,
            set_flags,
            rd: if set_flags {
                RegSel::gp(rd)
            } else {
                RegSel::sp(rd)
            },
            rn: RegSel::sp(rn),
            rm: RegSel::gp(rm),
            ext: Extend::from_bits(bits(raw, 15, 13)),
            amount,
        };
    }

    // The remaining families all have bits[28:21] as their discriminator.
    match bits(raw, 28, 21) {
        // Add/subtract (with carry).
        0b1101_0000 => {
            if bits(raw, 15, 10) != 0 {
                return Inst::Undefined { raw };
            }
            Inst::AddSubCarry {
                sz,
                op: if bit(raw, 30) == 0 { AddOp::Add } else { AddOp::Sub },
                set_flags: bit(raw, 29) == 1,
                rd: RegSel::gp(rd),
                rn: RegSel::gp(rn),
                rm: RegSel::gp(rm),
            }
        }
        // Conditional compare (register / immediate).
        0b1101_0010 => {
            if bit(raw, 29) != 1 || bit(raw, 10) != 0 || bit(raw, 4) != 0 {
                return Inst::Undefined { raw };
            }
            let imm_form = bit(raw, 11) == 1;
            Inst::CondCompare {
                sz,
                op: if bit(raw, 30) == 0 { AddOp::Add } else { AddOp::Sub },
                rn: RegSel::gp(rn),
                rm: if imm_form {
                    None
                } else {
                    Some(RegSel::gp(rm))
                },
                imm: rm as u64,
                nzcv: bits(raw, 3, 0) as u8,
                cond: Cond(bits(raw, 15, 12) as u8),
            }
        }
        // Conditional select.
        0b1101_0100 => {
            if bit(raw, 29) != 0 || bit(raw, 11) != 0 {
                return Inst::Undefined { raw };
            }
            Inst::CondSelect {
                sz,
                rd: RegSel::gp(rd),
                rn: RegSel::gp(rn),
                rm: RegSel::gp(rm),
                cond: Cond(bits(raw, 15, 12) as u8),
                else_inc: bit(raw, 10) == 1,
                else_inv: bit(raw, 30) == 1,
            }
        }
        // Data processing (1 source) / (2 source).
        0b1101_0110 => {
            if bit(raw, 30) == 1 {
                // 1 source: opcode2 in [20:16], opcode in [15:10].
                if bit(raw, 29) != 0 || rm != 0 {
                    return Inst::Undefined { raw };
                }
                let op = match bits(raw, 15, 10) {
                    0b000000 => Dp1Op::Rbit,
                    0b000001 => Dp1Op::Rev16,
                    0b000010 => {
                        if sf == 1 {
                            Dp1Op::Rev32
                        } else {
                            Dp1Op::Rev
                        }
                    }
                    0b000011 if sf == 1 => Dp1Op::Rev,
                    0b000100 => Dp1Op::Clz,
                    0b000101 => Dp1Op::Cls,
                    _ => return Inst::Unknown { raw },
                };
                Inst::DataProc1 {
                    sz,
                    op,
                    rd: RegSel::gp(rd),
                    rn: RegSel::gp(rn),
                }
            } else {
                if bit(raw, 29) != 0 {
                    return Inst::Undefined { raw };
                }
                let op = match bits(raw, 15, 10) {
                    0b000010 => Dp2Op::Udiv,
                    0b000011 => Dp2Op::Sdiv,
                    0b001000 => Dp2Op::Lslv,
                    0b001001 => Dp2Op::Lsrv,
                    0b001010 => Dp2Op::Asrv,
                    0b001011 => Dp2Op::Rorv,
                    0b010000 if sf == 0 => Dp2Op::Crc32b,
                    0b010001 if sf == 0 => Dp2Op::Crc32h,
                    0b010010 if sf == 0 => Dp2Op::Crc32w,
                    0b010011 if sf == 1 => Dp2Op::Crc32x,
                    0b010100 if sf == 0 => Dp2Op::Crc32cb,
                    0b010101 if sf == 0 => Dp2Op::Crc32ch,
                    0b010110 if sf == 0 => Dp2Op::Crc32cw,
                    0b010111 if sf == 1 => Dp2Op::Crc32cx,
                    _ => return Inst::Unknown { raw },
                };
                Inst::DataProc2 {
                    sz,
                    op,
                    rd: RegSel::gp(rd),
                    rn: RegSel::gp(rn),
                    rm: RegSel::gp(rm),
                }
            }
        }
        _ => {
            // Data processing (3 source): sf op54 11011 op31 Rm o0 Ra Rn Rd
            if bits(raw, 28, 24) == 0b11011 {
                if bits(raw, 30, 29) != 0 {
                    return Inst::Undefined { raw };
                }
                let op31 = bits(raw, 23, 21);
                let o0 = bit(raw, 15);
                let op = match (sf, op31, o0) {
                    (_, 0b000, 0) => Dp3Op::Madd,
                    (_, 0b000, 1) => Dp3Op::Msub,
                    (1, 0b001, 0) => Dp3Op::Smaddl,
                    (1, 0b001, 1) => Dp3Op::Smsubl,
                    (1, 0b010, 0) => Dp3Op::Smulh,
                    (1, 0b101, 0) => Dp3Op::Umaddl,
                    (1, 0b101, 1) => Dp3Op::Umsubl,
                    (1, 0b110, 0) => Dp3Op::Umulh,
                    _ => return Inst::Undefined { raw },
                };
                return Inst::DataProc3 {
                    sz,
                    op,
                    rd: RegSel::gp(rd),
                    rn: RegSel::gp(rn),
                    rm: RegSel::gp(rm),
                    ra: RegSel::gp(bits(raw, 14, 10)),
                };
            }
            Inst::Unknown { raw }
        }
    }
}

// ---------------------------------------------------------------------------
// Loads and stores
// ---------------------------------------------------------------------------

fn decode_load_store(raw: u32, pc: u64) -> Inst {
    // Load/store exclusive: size 001000 o2 L o1 Rs o0 Rt2 Rn Rt
    if bits(raw, 29, 24) == 0b001000 {
        return decode_ldst_exclusive(raw);
    }
    // Load register (literal): opc 011 V 00 imm19 Rt
    if bits(raw, 29, 27) == 0b011 && bits(raw, 25, 24) == 0b00 {
        let opc = bits(raw, 31, 30);
        let v = bit(raw, 26);
        let off = sext(bits(raw, 23, 5), 19) << 2;
        let addr = AddrMode::Literal {
            addr: pc.wrapping_add(off as u64),
        };
        if v == 1 {
            // SIMD&FP literal load — deferred to the NEON milestone.
            return Inst::Unknown { raw };
        }
        return match opc {
            0b00 => Inst::Load {
                sz: MemSz::W,
                dest: Sz::W,
                signed: false,
                rt: RegSel::gp(bits(raw, 4, 0)),
                addr,
                order: Ordering::None,
            },
            0b01 => Inst::Load {
                sz: MemSz::D,
                dest: Sz::X,
                signed: false,
                rt: RegSel::gp(bits(raw, 4, 0)),
                addr,
                order: Ordering::None,
            },
            0b10 => Inst::Load {
                sz: MemSz::W,
                dest: Sz::X,
                signed: true,
                rt: RegSel::gp(bits(raw, 4, 0)),
                addr,
                order: Ordering::None,
            },
            // PRFM (literal).
            _ => Inst::Prefetch { addr },
        };
    }
    // Load/store pair: opc 101 V idx L imm7 Rt2 Rn Rt
    if bits(raw, 29, 27) == 0b101 {
        return decode_ldst_pair(raw);
    }
    // Load/store register, all remaining addressing modes: size 111 V ...
    if bits(raw, 29, 27) == 0b111 {
        if bits(raw, 25, 24) == 0b01 {
            return decode_ldst_uimm(raw);
        }
        if bits(raw, 25, 24) == 0b00 {
            if bit(raw, 21) == 1 {
                if bits(raw, 11, 10) == 0b10 {
                    return decode_ldst_regoff(raw);
                }
                if bits(raw, 11, 10) == 0b00 && bit(raw, 26) == 0 {
                    return decode_atomic_memop(raw);
                }
                return Inst::Unknown { raw };
            }
            return decode_ldst_imm9(raw);
        }
        return Inst::Unknown { raw };
    }
    // Advanced SIMD load/store multiple structures (LD1/ST1/…) and the
    // remaining `x1x0` sub-groups.
    Inst::Unknown { raw }
}

/// Interpret the `size`/`opc` pair shared by the register load/store forms.
///
/// Returns `None` for SIMD&FP variants and reserved combinations.
enum LdSt {
    Load {
        sz: MemSz,
        dest: Sz,
        signed: bool,
    },
    Store {
        sz: MemSz,
    },
    Prefetch,
}

fn classify_ldst(size: u32, opc: u32, v: u32) -> Option<LdSt> {
    if v == 1 {
        // SIMD&FP load/store — NEON milestone.
        return None;
    }
    let msz = MemSz::from_log2(size);
    match opc {
        0b00 => Some(LdSt::Store { sz: msz }),
        0b01 => Some(LdSt::Load {
            sz: msz,
            dest: if size == 0b11 { Sz::X } else { Sz::W },
            signed: false,
        }),
        0b10 => {
            if size == 0b11 {
                // PRFM (unsigned/register offset forms).
                Some(LdSt::Prefetch)
            } else {
                Some(LdSt::Load {
                    sz: msz,
                    dest: Sz::X,
                    signed: true,
                })
            }
        }
        _ => {
            if size == 0b11 {
                None
            } else {
                Some(LdSt::Load {
                    sz: msz,
                    dest: Sz::W,
                    signed: true,
                })
            }
        }
    }
}

fn build_ldst(kind: LdSt, rt: RegSel, addr: AddrMode, order: Ordering, raw: u32) -> Inst {
    match kind {
        LdSt::Load { sz, dest, signed } => Inst::Load {
            sz,
            dest,
            signed,
            rt,
            addr,
            order,
        },
        LdSt::Store { sz } => Inst::Store {
            sz,
            rt,
            addr,
            order,
        },
        LdSt::Prefetch => {
            let _ = raw;
            Inst::Prefetch { addr }
        }
    }
}

fn decode_ldst_uimm(raw: u32) -> Inst {
    let size = bits(raw, 31, 30);
    let v = bit(raw, 26);
    let opc = bits(raw, 23, 22);
    let Some(kind) = classify_ldst(size, opc, v) else {
        return Inst::Unknown { raw };
    };
    // The 12-bit immediate is scaled by the access size.
    let off = (bits(raw, 21, 10) as i64) << size;
    let addr = AddrMode::Offset {
        rn: RegSel::sp(bits(raw, 9, 5)),
        off,
    };
    build_ldst(kind, RegSel::gp(bits(raw, 4, 0)), addr, Ordering::None, raw)
}

fn decode_ldst_imm9(raw: u32) -> Inst {
    let size = bits(raw, 31, 30);
    let v = bit(raw, 26);
    let opc = bits(raw, 23, 22);
    let Some(kind) = classify_ldst(size, opc, v) else {
        return Inst::Unknown { raw };
    };
    let off = sext(bits(raw, 20, 12), 9);
    let rn = RegSel::sp(bits(raw, 9, 5));
    // bits[11:10]: 00 unscaled, 01 post-index, 10 unprivileged, 11 pre-index.
    let addr = match bits(raw, 11, 10) {
        0b00 | 0b10 => AddrMode::Offset { rn, off },
        0b01 => AddrMode::PostIndex { rn, off },
        _ => AddrMode::PreIndex { rn, off },
    };
    build_ldst(kind, RegSel::gp(bits(raw, 4, 0)), addr, Ordering::None, raw)
}

fn decode_ldst_regoff(raw: u32) -> Inst {
    let size = bits(raw, 31, 30);
    let v = bit(raw, 26);
    let opc = bits(raw, 23, 22);
    let Some(kind) = classify_ldst(size, opc, v) else {
        return Inst::Unknown { raw };
    };
    let option = bits(raw, 15, 13);
    if option & 0b010 == 0 {
        // option<1> must be 1; otherwise UNDEFINED.
        return Inst::Undefined { raw };
    }
    let s = bit(raw, 12);
    let amount = if s == 1 { size } else { 0 };
    let addr = AddrMode::RegOffset {
        rn: RegSel::sp(bits(raw, 9, 5)),
        rm: RegSel::gp(bits(raw, 20, 16)),
        ext: Extend::from_bits(option),
        amount,
    };
    build_ldst(kind, RegSel::gp(bits(raw, 4, 0)), addr, Ordering::None, raw)
}

fn decode_ldst_pair(raw: u32) -> Inst {
    let opc = bits(raw, 31, 30);
    let v = bit(raw, 26);
    let load = bit(raw, 22) == 1;
    if v == 1 {
        return Inst::Unknown { raw };
    }
    let (sz, signed) = match opc {
        0b00 => (MemSz::W, false),
        0b01 => {
            if !load {
                return Inst::Undefined { raw };
            }
            (MemSz::W, true) // LDPSW
        }
        0b10 => (MemSz::D, false),
        _ => return Inst::Undefined { raw },
    };
    let scale = sz.log2();
    let off = sext(bits(raw, 21, 15), 7) << scale;
    let rn = RegSel::sp(bits(raw, 9, 5));
    let addr = match bits(raw, 25, 23) {
        // 000 = no-allocate (STNP/LDNP), same addressing as the offset form.
        0b000 | 0b010 => AddrMode::Offset { rn, off },
        0b001 => AddrMode::PostIndex { rn, off },
        0b011 => AddrMode::PreIndex { rn, off },
        _ => return Inst::Unknown { raw },
    };
    let rt = RegSel::gp(bits(raw, 4, 0));
    let rt2 = RegSel::gp(bits(raw, 14, 10));
    if load {
        Inst::LoadPair {
            sz,
            signed,
            rt,
            rt2,
            addr,
        }
    } else {
        Inst::StorePair { sz, rt, rt2, addr }
    }
}

fn decode_ldst_exclusive(raw: u32) -> Inst {
    let size = bits(raw, 31, 30);
    let o2 = bit(raw, 23);
    let l = bit(raw, 22);
    let o1 = bit(raw, 21);
    let rs = bits(raw, 20, 16);
    let o0 = bit(raw, 15);
    let rn = bits(raw, 9, 5);
    let rt = bits(raw, 4, 0);
    let msz = MemSz::from_log2(size);

    match (o2, o1) {
        // Load/store exclusive register (LDXR/LDAXR/STXR/STLXR).
        (0, 0) => {
            let order = if o0 == 1 {
                if l == 1 {
                    Ordering::Acquire
                } else {
                    Ordering::Release
                }
            } else {
                Ordering::None
            };
            if l == 1 {
                Inst::LoadExclusive {
                    sz: msz,
                    rt: RegSel::gp(rt),
                    rn: RegSel::sp(rn),
                    order,
                }
            } else {
                Inst::StoreExclusive {
                    sz: msz,
                    rs: RegSel::gp(rs),
                    rt: RegSel::gp(rt),
                    rn: RegSel::sp(rn),
                    order,
                }
            }
        }
        // Exclusive pair (LDXP/STXP) and CASP — deferred to the threading
        // milestone; nothing in T0/T1 userspace emits them.
        (0, 1) => Inst::Unknown { raw },
        // Load-acquire / store-release register — ordered but not exclusive.
        // Covers LDAR/STLR (o0=1) and LDLAR/STLLR (o0=0, FEAT_LOR); treating
        // the LOR forms as full acquire/release is stronger, hence safe.
        (1, 0) => {
            if rs != 0b11111 || bits(raw, 14, 10) != 0b11111 {
                return Inst::Unknown { raw };
            }
            if l == 1 {
                Inst::Load {
                    sz: msz,
                    dest: if size == 0b11 { Sz::X } else { Sz::W },
                    signed: false,
                    rt: RegSel::gp(rt),
                    addr: AddrMode::Offset {
                        rn: RegSel::sp(rn),
                        off: 0,
                    },
                    order: Ordering::Acquire,
                }
            } else {
                Inst::Store {
                    sz: msz,
                    rt: RegSel::gp(rt),
                    addr: AddrMode::Offset {
                        rn: RegSel::sp(rn),
                        off: 0,
                    },
                    order: Ordering::Release,
                }
            }
        }
        // CAS/CASA/CASL/CASAL: L is the acquire bit, o0 the release bit.
        _ => {
            if bits(raw, 14, 10) != 0b11111 {
                return Inst::Unknown { raw };
            }
            Inst::CompareSwap {
                sz: msz,
                rs: RegSel::gp(rs),
                rt: RegSel::gp(rt),
                rn: RegSel::sp(rn),
                acquire: l == 1,
                release: o0 == 1,
            }
        }
    }
}

fn decode_atomic_memop(raw: u32) -> Inst {
    // size 111 0 00 A R 1 Rs o3 opc 00 Rn Rt
    let size = bits(raw, 31, 30);
    let a = bit(raw, 23);
    let r = bit(raw, 22);
    let rs = bits(raw, 20, 16);
    let o3 = bit(raw, 15);
    let opc = bits(raw, 14, 12);
    let rn = bits(raw, 9, 5);
    let rt = bits(raw, 4, 0);
    let msz = MemSz::from_log2(size);

    let op = match (o3, opc) {
        (0, 0b000) => RmwOp::Add,
        (0, 0b001) => RmwOp::Clr,
        (0, 0b010) => RmwOp::Eor,
        (0, 0b011) => RmwOp::Set,
        (0, 0b100) => RmwOp::Smax,
        (0, 0b101) => RmwOp::Smin,
        (0, 0b110) => RmwOp::Umax,
        (0, 0b111) => RmwOp::Umin,
        (1, 0b000) => RmwOp::Swp,
        _ => return Inst::Unknown { raw },
    };
    Inst::AtomicRmw {
        sz: msz,
        op,
        rs: RegSel::gp(rs),
        rt: RegSel::gp(rt),
        rn: RegSel::sp(rn),
        acquire: a == 1,
        release: r == 1,
    }
}

// ---------------------------------------------------------------------------
// SIMD & FP — decoded as Unknown until the NEON milestone (PLAN M2).
// ---------------------------------------------------------------------------

fn decode_simd_fp(raw: u32) -> Inst {
    Inst::Unknown { raw }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(raw: u32) -> Inst {
        decode(raw, 0x1000)
    }

    #[test]
    fn nop_and_ret() {
        assert_eq!(d(0xd503201f), Inst::Hint { imm: 0 });
        assert_eq!(
            d(0xd65f03c0),
            Inst::BranchReg {
                rn: RegSel::Gp(30),
                link: false,
                is_ret: true
            }
        );
    }

    #[test]
    fn mov_immediate() {
        // mov x0, #1  ->  movz x0, #1
        assert_eq!(
            d(0xd2800020),
            Inst::MoveWide {
                sz: Sz::X,
                op: MoveOp::Movz,
                rd: RegSel::Gp(0),
                imm16: 1,
                shift: 0
            }
        );
    }

    #[test]
    fn add_immediate_uses_sp_form() {
        // sub sp, sp, #0x10
        let i = d(0xd10043ff);
        assert_eq!(
            i,
            Inst::AddSubImm {
                sz: Sz::X,
                op: AddOp::Sub,
                set_flags: false,
                rd: RegSel::Sp,
                rn: RegSel::Sp,
                imm: 0x10
            }
        );
    }

    #[test]
    fn cmp_uses_zr_destination() {
        // cmp x0, #0  ->  subs xzr, x0, #0
        assert_eq!(
            d(0xf100001f),
            Inst::AddSubImm {
                sz: Sz::X,
                op: AddOp::Sub,
                set_flags: true,
                rd: RegSel::Zr,
                rn: RegSel::Gp(0),
                imm: 0
            }
        );
    }

    #[test]
    fn branches_resolve_absolute_targets() {
        // b .+8 from pc=0x1000
        assert_eq!(
            d(0x14000002),
            Inst::Branch {
                target: 0x1008,
                link: false
            }
        );
        // bl .-8
        assert_eq!(
            d(0x97fffffe),
            Inst::Branch {
                target: 0x0ff8,
                link: true
            }
        );
        // b.eq .+16
        assert_eq!(
            d(0x54000080),
            Inst::BranchCond {
                cond: Cond::EQ,
                target: 0x1010
            }
        );
    }

    #[test]
    fn logical_immediate_expansion() {
        // and x0, x0, #0xfff
        match d(0x92402c00) {
            Inst::LogicalImm { imm, op, sz, .. } => {
                assert_eq!(imm, 0xfff);
                assert_eq!(op, LogOp::And);
                assert_eq!(sz, Sz::X);
            }
            other => panic!("{other:?}"),
        }
        // and x0, x0, #0xffff
        match d(0x92403c00) {
            Inst::LogicalImm { imm, .. } => assert_eq!(imm, 0xffff),
            other => panic!("{other:?}"),
        }
        // tst x1, #0xf0  ->  ands xzr, x1, #0xf0
        match d(0xf27c0c3f) {
            Inst::LogicalImm {
                imm,
                rd,
                set_flags,
                ..
            } => {
                assert_eq!(imm, 0xf0);
                assert_eq!(rd, RegSel::Zr);
                assert!(set_flags);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bitmask_reserved_encodings() {
        // imms == levels is reserved in the immediate form.
        assert_eq!(decode_bit_masks(1, 0b111111, 0, true), None);
        // N=1, imms=0 -> a single set bit, no replication.
        assert_eq!(decode_bit_masks(1, 0, 0, true), Some(1));
        // N=1, imms=31 -> the low 32 bits set, still one 64-bit element.
        assert_eq!(decode_bit_masks(1, 31, 0, true), Some(0xffff_ffff));
        // N=0, imms=0b011111 makes esize=32 with imms==levels: reserved.
        assert_eq!(decode_bit_masks(0, 0b011111, 0, true), None);
        // N=1 is undefined for 32-bit operands.
        assert_eq!(decode_bit_masks(1, 0, 0, false), None);
    }

    #[test]
    fn ldr_scaled_offset() {
        // ldr x1, [x0, #8]
        assert_eq!(
            d(0xf9400401),
            Inst::Load {
                sz: MemSz::D,
                dest: Sz::X,
                signed: false,
                rt: RegSel::Gp(1),
                addr: AddrMode::Offset {
                    rn: RegSel::Gp(0),
                    off: 8
                },
                order: Ordering::None,
            }
        );
    }

    #[test]
    fn stp_pre_index() {
        // stp x29, x30, [sp, #-16]!
        assert_eq!(
            d(0xa9bf7bfd),
            Inst::StorePair {
                sz: MemSz::D,
                rt: RegSel::Gp(29),
                rt2: RegSel::Gp(30),
                addr: AddrMode::PreIndex {
                    rn: RegSel::Sp,
                    off: -16
                },
            }
        );
    }

    #[test]
    fn svc_zero() {
        assert_eq!(d(0xd4000001), Inst::Svc { imm: 0 });
    }

    #[test]
    fn adrp_resolves_to_page_address() {
        // adrp x0, .  from pc=0x1234 -> page 0x1000
        match decode(0x90000000, 0x1234) {
            Inst::Adrp { rd, target } => {
                assert_eq!(rd, RegSel::Gp(0));
                assert_eq!(target, 0x1000);
            }
            other => panic!("{other:?}"),
        }
        // adr x1, .+12 from pc=0xa84 -> 0xa90
        match decode(0x10000061, 0xa84) {
            Inst::Adr { rd, target } => {
                assert_eq!(rd, RegSel::Gp(1));
                assert_eq!(target, 0xa90);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn decoder_is_total() {
        // Sample the encoding space; the only requirement is "no panic".
        let mut raw: u32 = 0x9e37_79b9;
        for _ in 0..200_000 {
            raw = raw.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let _ = decode(raw, 0x4000);
        }
    }
}
