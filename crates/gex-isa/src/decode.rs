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
            let op = if bit(raw, 30) == 0 {
                AddOp::Add
            } else {
                AddOp::Sub
            };
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
            ((op0 as u16) << 14)
                | ((op1 as u16) << 11)
                | ((crn as u16) << 7)
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
        let op = if bit(raw, 30) == 0 {
            AddOp::Add
        } else {
            AddOp::Sub
        };
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
                op: if bit(raw, 30) == 0 {
                    AddOp::Add
                } else {
                    AddOp::Sub
                },
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
                op: if bit(raw, 30) == 0 {
                    AddOp::Add
                } else {
                    AddOp::Sub
                },
                rn: RegSel::gp(rn),
                rm: if imm_form { None } else { Some(RegSel::gp(rm)) },
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
            _ => Inst::Prefetch {
                addr,
                hint: bits(raw, 4, 0) as u8,
            },
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
    // Advanced SIMD load/store multiple structures.
    if bit(raw, 31) == 0 && bits(raw, 29, 24) == 0b001100 {
        return decode_simd_ldst_multi(raw);
    }
    // Advanced SIMD load/store single structure.
    if bit(raw, 31) == 0 && bits(raw, 29, 24) == 0b001101 {
        return decode_simd_ldst_single(raw);
    }
    Inst::Unknown { raw }
}

/// `ld1`/`st1` (multiple structures), with and without post-index.
///
/// The interleaving variants (`ld2`/`ld3`/`ld4`) share this space but permute
/// lanes as they go, so only the `ld1`/`st1` opcodes are accepted here.
fn decode_simd_ldst_multi(raw: u32) -> Inst {
    let q = bit(raw, 30) == 1;
    let post_index = bit(raw, 23) == 1;
    let load = bit(raw, 22) == 1;
    let rm = bits(raw, 20, 16);
    let opcode = bits(raw, 15, 12);

    // The register count is encoded in the opcode rather than as a field.
    // `ld2`/`st2` (opcode 1000) also live here but interleave, so they get
    // their own instruction below.
    let regs: u8 = match opcode {
        0b0111 => 1,
        0b1010 => 2,
        0b0110 => 3,
        0b0010 => 4,
        0b1000 => 0,
        _ => return Inst::Unknown { raw },
    };
    if !post_index && rm != 0 {
        return Inst::Unknown { raw };
    }
    let bytes = if q { 16 } else { 8 };
    let rn = RegSel::sp(bits(raw, 9, 5));
    let addr = if post_index {
        // Rm == 31 means the immediate form: advance by the total size.
        let off = if rm == 0b11111 {
            // `ld2` reports zero registers but moves two.
            (if regs == 0 { 2 } else { regs as i64 }) * bytes as i64
        } else {
            // The register-offset post-index form needs a value the lifter
            // would have to read; it does not appear in compiler output.
            return Inst::Unknown { raw };
        };
        AddrMode::PostIndex { rn, off }
    } else {
        AddrMode::Offset { rn, off: 0 }
    };
    let vt = VReg(bits(raw, 4, 0) as u8);
    if regs == 0 {
        return Inst::VLoadStore2 {
            load,
            lane_bytes: 1 << bits(raw, 11, 10),
            bytes,
            vt,
            addr,
        };
    }
    if load {
        Inst::VLoadMulti {
            regs,
            vt,
            bytes,
            addr,
        }
    } else {
        Inst::VStoreMulti {
            regs,
            vt,
            bytes,
            addr,
        }
    }
}

/// `ld1`/`st1` on one lane, and `ld1r`.
///
/// The lane index is scattered across Q, S and `size` in a way that depends on
/// the element width, because a byte arrangement has sixteen lanes to name.
fn decode_simd_ldst_single(raw: u32) -> Inst {
    let q = bit(raw, 30) == 1;
    let post_index = bit(raw, 23) == 1;
    let load = bit(raw, 22) == 1;
    // R selects the paired `ld2`/`ld2r` forms, which are not handled here.
    if bit(raw, 21) != 0 {
        return Inst::Unknown { raw };
    }
    let rm = bits(raw, 20, 16);
    let opcode = bits(raw, 15, 13);
    let s = bit(raw, 12);
    let size = bits(raw, 11, 10);
    let qb = q as u32;

    let (lane_bytes, index, replicate) = match opcode {
        0b000 => (1, qb << 3 | s << 2 | size, false),
        0b010 if size & 1 == 0 => (2, qb << 2 | s << 1 | (size >> 1), false),
        0b100 if size == 0b00 => (4, qb << 1 | s, false),
        0b100 if size == 0b01 && s == 0 => (8, qb, false),
        // `ld1r`, which has no index: `size` is simply the element width.
        0b110 if load => (1 << size, 0, true),
        _ => return Inst::Unknown { raw },
    };
    if !post_index && rm != 0 {
        return Inst::Unknown { raw };
    }
    let rn = RegSel::sp(bits(raw, 9, 5));
    let addr = if post_index {
        // Only the immediate form appears in compiler output; it advances by
        // exactly the element just transferred.
        if rm != 0b11111 {
            return Inst::Unknown { raw };
        }
        AddrMode::PostIndex {
            rn,
            off: lane_bytes as i64,
        }
    } else {
        AddrMode::Offset { rn, off: 0 }
    };
    Inst::VLoadStoreLane {
        load,
        replicate,
        lane_bytes,
        index,
        q,
        vt: VReg(bits(raw, 4, 0) as u8),
        addr,
    }
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
    /// A SIMD&FP access of `bytes` bytes.
    VLoad {
        bytes: u32,
    },
    VStore {
        bytes: u32,
    },
}

fn classify_ldst(size: u32, opc: u32, v: u32) -> Option<LdSt> {
    if v == 1 {
        // SIMD&FP. `opc<1>:size` gives the access width and `opc<0>` the
        // direction, so a 16-byte access is `opc = 1x` with `size = 00`.
        let width_log2 = ((opc >> 1) << 2) | size;
        if width_log2 > 4 {
            return None;
        }
        let bytes = 1u32 << width_log2;
        return Some(if opc & 1 != 0 {
            LdSt::VLoad { bytes }
        } else {
            LdSt::VStore { bytes }
        });
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

/// Scale applied to a load/store's immediate offset: the log2 of the access
/// width. For SIMD&FP that is *not* the `size` field, because a 16-byte
/// access encodes `size = 00`.
fn ldst_scale(kind: &LdSt) -> u32 {
    match kind {
        LdSt::Load { sz, .. } | LdSt::Store { sz } => sz.log2(),
        LdSt::VLoad { bytes } | LdSt::VStore { bytes } => bytes.trailing_zeros(),
        LdSt::Prefetch => 3,
    }
}

fn build_ldst(kind: LdSt, rt: RegSel, addr: AddrMode, order: Ordering, raw: u32) -> Inst {
    let vreg = VReg((raw & 31) as u8);
    match kind {
        LdSt::VLoad { bytes } => Inst::VLoad {
            bytes,
            vt: vreg,
            addr,
        },
        LdSt::VStore { bytes } => Inst::VStore {
            bytes,
            vt: vreg,
            addr,
        },
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
        LdSt::Prefetch => Inst::Prefetch {
            addr,
            hint: bits(raw, 4, 0) as u8,
        },
    }
}

fn decode_ldst_uimm(raw: u32) -> Inst {
    let size = bits(raw, 31, 30);
    let v = bit(raw, 26);
    let opc = bits(raw, 23, 22);
    let Some(kind) = classify_ldst(size, opc, v) else {
        return Inst::Unknown { raw };
    };
    // The 12-bit immediate is scaled by the access width.
    let off = (bits(raw, 21, 10) as i64) << ldst_scale(&kind);
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
    let amount = if s == 1 { ldst_scale(&kind) } else { 0 };
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
        // SIMD&FP pair: opc selects 4-, 8- or 16-byte registers.
        let bytes = match opc {
            0b00 => 4u32,
            0b01 => 8,
            0b10 => 16,
            _ => return Inst::Undefined { raw },
        };
        let scale = bytes.trailing_zeros();
        let off = sext(bits(raw, 21, 15), 7) << scale;
        let rn = RegSel::sp(bits(raw, 9, 5));
        let addr = match bits(raw, 25, 23) {
            0b000 | 0b010 => AddrMode::Offset { rn, off },
            0b001 => AddrMode::PostIndex { rn, off },
            0b011 => AddrMode::PreIndex { rn, off },
            _ => return Inst::Unknown { raw },
        };
        let vt = VReg(bits(raw, 4, 0) as u8);
        let vt2 = VReg(bits(raw, 14, 10) as u8);
        return if load {
            Inst::VLoadPair {
                bytes,
                vt,
                vt2,
                addr,
            }
        } else {
            Inst::VStorePair {
                bytes,
                vt,
                vt2,
                addr,
            }
        };
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
    let q = bit(raw, 30) == 1;
    let u = bit(raw, 29);

    // Advanced SIMD copy: 0 Q op 01110000 imm5 0 imm4 1 Rn Rd, and its
    // scalar sibling, which differs only in bit 28 and takes the single-lane
    // `dup` out of it.
    if bit(raw, 31) == 0
        && bits(raw, 27, 21) == 0b111_0000
        && bit(raw, 15) == 0
        && bit(raw, 10) == 1
    {
        let scalar = bit(raw, 28) == 1;
        if !scalar || (q && u == 0 && bits(raw, 14, 11) == 0) {
            return decode_simd_copy(raw, q, u, scalar);
        }
    }

    // Advanced SIMD modified immediate: 0 Q op 0111100000 abc cmode o2 1 defgh Rd
    if bit(raw, 31) == 0
        && bits(raw, 28, 24) == 0b01111
        && bits(raw, 23, 19) == 0
        && bit(raw, 10) == 1
    {
        let cmode = bits(raw, 15, 12);
        let imm8 = (bits(raw, 18, 16) << 5) | bits(raw, 9, 5);
        // An odd `cmode` below 1100 is not a move at all: it reads the
        // destination and combines the immediate into it. `orr` for op=0 and
        // `bic` for op=1. This is checked before the move expansion, which
        // has its own idea of what an inverted immediate means and rejects
        // these outright.
        if cmode & 1 == 1 && cmode < 0b1100 {
            // The immediate expands exactly as the move one bit below does:
            // `cmode` 0xx1 is the 32-bit-lane shifted form, 10x1 the
            // 16-bit-lane one. Asking for the odd `cmode` itself gets
            // nothing, because the expander only knows the move encodings.
            let Some(plain) = expand_simd_immediate(imm8, cmode & !1, false, q) else {
                return Inst::Unknown { raw };
            };
            return Inst::VLogicalImm {
                vd: VReg(bits(raw, 4, 0) as u8),
                value: plain,
                clear: u == 1,
                q,
            };
        }
        let Some(value) = expand_simd_immediate(imm8, cmode, u == 1, q) else {
            return Inst::Unknown { raw };
        };
        return Inst::VMoveImm {
            vd: VReg(bits(raw, 4, 0) as u8),
            value,
            // `cmode` 1110 is `movi`/`movi`-of-bytes and 1111 is `fmov`;
            // only the numeric families below them have an inverting form.
            inverted: u == 1 && cmode < 0b1110,
            fp_lane_bytes: (cmode == 0b1111).then(|| if u == 1 { 8 } else { 4 }),
            cmode: cmode as u8,
            imm8: imm8 as u8,
            q,
            op: u == 1,
        };
    }

    // Advanced SIMD vector x indexed element: 0 Q U 01111 size L M Rm opcode
    // H 0 Rn Rd. Bit 10 is what separates it from the shift-by-immediate and
    // modified-immediate groups above, which both set it.
    if bit(raw, 31) == 0 && bits(raw, 28, 24) == 0b01111 && bit(raw, 10) == 0 {
        if let Some(inst) = decode_simd_by_element(raw, q, u) {
            return inst;
        }
    }

    // Advanced SIMD shift by immediate: 0 Q U 011110 immh immb opcode 1 Rn Rd
    if bit(raw, 31) == 0 && bits(raw, 28, 23) == 0b011110 && bit(raw, 10) == 1 {
        let immh = bits(raw, 22, 19);
        if immh != 0 {
            return decode_simd_shift_imm(raw, q, u, immh, bits(raw, 18, 16));
        }
    }

    // Advanced SIMD three different: 0 Q U 01110 size 1 Rm opcode 00 Rn Rd.
    // Bits 11:10 being zero is what marks it out from the three-same group,
    // which shares everything above them.
    if bit(raw, 31) == 0
        && bits(raw, 28, 24) == 0b01110
        && bit(raw, 21) == 1
        && bits(raw, 11, 10) == 0
    {
        if let Some(inst) = decode_simd_three_different(raw, q, u) {
            return inst;
        }
    }

    // The three-same, two-register-misc, across-lanes, permute, extract and
    // table-lookup groups all live under `0 Q U 01110 ...`.
    if bit(raw, 31) == 0 && bits(raw, 28, 24) == 0b01110 {
        // Extract: 0 Q 101110 op2 0 Rm 0 imm4 0 Rn Rd
        if u == 1
            && bits(raw, 23, 22) == 0
            && bit(raw, 21) == 0
            && bit(raw, 15) == 0
            && bit(raw, 10) == 0
        {
            let index = bits(raw, 14, 11);
            // With Q clear the window is eight bytes wide, so the top bit of
            // imm4 must be zero; the architecture leaves the rest undefined.
            // Exhaustive discovery decodes literal pools and padding, so this
            // does come up -- and an out-of-range window would become an
            // `i8x16.shuffle` whose lane indices exceed 31, which V8 rejects
            // when it compiles the *module*, poisoning code that is never run.
            if !q && index >= 8 {
                return Inst::Undefined { raw };
            }
            return Inst::VExt {
                vd: VReg(bits(raw, 4, 0) as u8),
                vn: VReg(bits(raw, 9, 5) as u8),
                vm: VReg(bits(raw, 20, 16) as u8),
                index,
                q,
            };
        }
        // Table lookup: 0 Q 001110 op2 0 Rm 0 len op 00 Rn Rd
        if u == 0
            && bits(raw, 23, 22) == 0
            && bit(raw, 21) == 0
            && bit(raw, 15) == 0
            && bits(raw, 11, 10) == 0
        {
            return Inst::VTableLookup {
                vd: VReg(bits(raw, 4, 0) as u8),
                vn: VReg(bits(raw, 9, 5) as u8),
                regs: bits(raw, 14, 13) as u8 + 1,
                vm: VReg(bits(raw, 20, 16) as u8),
                q,
                is_tbx: bit(raw, 12) == 1,
            };
        }
        // Permute: 0 Q 001110 size 0 Rm 0 opcode 10 Rn Rd
        if bit(raw, 21) == 0 && bit(raw, 15) == 0 && bits(raw, 11, 10) == 0b10 {
            let op = match bits(raw, 14, 12) {
                0b001 => VPermOp::Uzp1,
                0b010 => VPermOp::Trn1,
                0b011 => VPermOp::Zip1,
                0b101 => VPermOp::Uzp2,
                0b110 => VPermOp::Trn2,
                0b111 => VPermOp::Zip2,
                _ => return Inst::Unknown { raw },
            };
            let Some(lane_bytes) = lane_from_size(bits(raw, 23, 22)) else {
                return Inst::Undefined { raw };
            };
            return Inst::VPermute {
                op,
                arr: VArr {
                    lane_bytes,
                    q,
                    float: false,
                    scalar: false,
                },
                vd: VReg(bits(raw, 4, 0) as u8),
                vn: VReg(bits(raw, 9, 5) as u8),
                vm: VReg(bits(raw, 20, 16) as u8),
            };
        }
        if bit(raw, 21) == 1 {
            if bits(raw, 21, 17) == 0b10000 && bits(raw, 11, 10) == 0b10 {
                return decode_simd_two_misc(raw, q, u);
            }
            if bits(raw, 21, 17) == 0b11000 && bits(raw, 11, 10) == 0b10 {
                return decode_simd_across(raw, q, u);
            }
            if bit(raw, 10) == 1 {
                return decode_simd_three_same(raw, q, u);
            }

        }
    }

    // Scalar Advanced SIMD. Bit 30 is what separates it from scalar floating
    // point, which shares the `11110` group: the SIMD forms set it, the
    // floating-point ones clear it. The group is matched only down to bit 25,
    // because the shift-by-immediate forms sit at `11111` rather than
    // `11110` -- testing bit 24 here would hide the whole family, which is
    // what made `shl d1, d1, #52` undecodable.
    if bit(raw, 31) == 0 && bit(raw, 30) == 1 && bits(raw, 28, 25) == 0b1111 {
        if let Some(inst) = decode_simd_scalar(raw, u) {
            return inst;
        }
    }

    decode_scalar_fp(raw)
}

/// The scalar members of the Advanced SIMD families.
///
/// Every one operates on lane 0 and zeroes the rest of the register, so they
/// reuse the vector decoders with a scalar arrangement and the lifter masks
/// the result. Returning `None` falls through to the floating-point decoder
/// rather than claiming an encoding this does not model.
fn decode_simd_scalar(raw: u32, u: u32) -> Option<Inst> {
    let scalarise = |mut inst: Inst| -> Inst {
        match &mut inst {
            Inst::VThreeSame { arr, .. }
            | Inst::VTwoMisc { arr, .. }
            | Inst::VShiftImm { arr, .. } => arr.scalar = true,
            _ => {}
        }
        inst
    };

    // Shift by immediate: 01 U 111110 immh immb opcode 1 Rn Rd
    if bits(raw, 28, 23) == 0b111110 && bit(raw, 10) == 1 {
        let immh = bits(raw, 22, 19);
        if immh != 0 {
            let inst = decode_simd_shift_imm(raw, true, u, immh, bits(raw, 18, 16));
            return matches!(inst, Inst::VShiftImm { .. }).then(|| scalarise(inst));
        }
        return None;
    }
    if bits(raw, 28, 24) != 0b11110 || bit(raw, 21) != 1 {
        return None;
    }
    // Two-register misc: bits 21:17 == 10000.
    if bits(raw, 21, 17) == 0b10000 && bits(raw, 11, 10) == 0b10 {
        let inst = decode_simd_two_misc(raw, true, u);
        return matches!(inst, Inst::VTwoMisc { .. }).then(|| scalarise(inst));
    }
    // Pairwise: bits 21:17 == 11000. Only the two-lane reductions exist here,
    // which is what `addp d0, v0.2d` and the floating-point `faddp` are.
    if bits(raw, 21, 17) == 0b11000 && bits(raw, 11, 10) == 0b10 {
        let opcode = bits(raw, 16, 12);
        let (op, arr) = match (u, opcode) {
            (0, 0b11011) => (
                VAcrossOp::Add,
                VArr {
                    lane_bytes: 8,
                    q: true,
                    float: false,
                    scalar: true,
                },
            ),
            (1, 0b01101) => (
                VAcrossOp::Add,
                VArr {
                    lane_bytes: if bit(raw, 22) == 1 { 8 } else { 4 },
                    q: true,
                    float: true,
                    scalar: true,
                },
            ),
            (1, 0b01111 | 0b01100) => (
                match (opcode, bit(raw, 23)) {
                    (0b01111, 0) => VAcrossOp::FMax,
                    (0b01111, _) => VAcrossOp::FMin,
                    (_, 0) => VAcrossOp::FMaxNm,
                    (_, _) => VAcrossOp::FMinNm,
                },
                VArr {
                    lane_bytes: if bit(raw, 22) == 1 { 8 } else { 4 },
                    q: true,
                    float: true,
                    scalar: true,
                },
            ),
            _ => return None,
        };
        return Some(Inst::VAcross {
            op,
            arr,
            vd: VReg(bits(raw, 4, 0) as u8),
            vn: VReg(bits(raw, 9, 5) as u8),
        });
    }
    // Three-same.
    if bit(raw, 10) == 1 {
        let inst = decode_simd_three_same(raw, true, u);
        return matches!(inst, Inst::VThreeSame { .. }).then(|| scalarise(inst));
    }
    None
}

/// Lane width in bytes from a two-bit `size` field.
fn lane_from_size(size: u32) -> Option<u32> {
    match size {
        0b00 => Some(1),
        0b01 => Some(2),
        0b10 => Some(4),
        _ => Some(8),
    }
}

fn decode_simd_copy(raw: u32, q: bool, u: u32, scalar: bool) -> Inst {
    let imm5 = bits(raw, 20, 16);
    let imm4 = bits(raw, 14, 11);

    // Lane moves between a general register and a vector.
    // imm4: 0011 = INS (general), 0101 = SMOV, 0111 = UMOV.
    if matches!(imm4, 0b0011 | 0b0101 | 0b0111) {
        let Some((lane_bytes, lane)) = decode_lane(imm5) else {
            return Inst::Undefined { raw };
        };
        // All of DUP, INS (general), SMOV and UMOV have op == 0; only
        // INS (element) sets it.
        if u != 0 {
            return Inst::Unknown { raw };
        }
        let to_vector = imm4 == 0b0011;
        let signed = imm4 == 0b0101;
        if !to_vector && lane_bytes == 8 && !q {
            return Inst::Undefined { raw };
        }
        // For `ins` the source register's width follows the element, not Q,
        // which the encoding leaves free. `smov`/`umov` do use Q.
        let int_sz = if to_vector {
            if lane_bytes == 8 {
                Sz::X
            } else {
                Sz::W
            }
        } else if q {
            Sz::X
        } else {
            Sz::W
        };
        return Inst::VMoveLane {
            to_vector,
            signed,
            lane_bytes,
            lane,
            int_sz,
            rd: RegSel::gp(if to_vector {
                bits(raw, 9, 5)
            } else {
                bits(raw, 4, 0)
            }),
            vd: VReg(if to_vector {
                bits(raw, 4, 0) as u8
            } else {
                bits(raw, 9, 5) as u8
            }),
        };
    }
    // INS (element): `op` is 1 and imm4 holds the source lane index.
    if u == 1 {
        let Some((lane_bytes, dst)) = decode_lane(imm5) else {
            return Inst::Undefined { raw };
        };
        let shift = lane_bytes.trailing_zeros();
        return Inst::VInsElement {
            vd: VReg(bits(raw, 4, 0) as u8),
            vn: VReg(bits(raw, 9, 5) as u8),
            lane_bytes,
            dst_index: dst,
            src_index: imm4 >> shift,
        };
    }
    match imm4 {
        0b0001 => {
            let Some((lane_bytes, _)) = decode_lane(imm5) else {
                return Inst::Undefined { raw };
            };
            if lane_bytes == 8 && !q {
                return Inst::Undefined { raw };
            }
            Inst::VDupGeneral {
                vd: VReg(bits(raw, 4, 0) as u8),
                rn: RegSel::gp(bits(raw, 9, 5)),
                lane_bytes,
                q,
            }
        }
        0b0000 => {
            let Some((lane_bytes, index)) = decode_lane(imm5) else {
                return Inst::Undefined { raw };
            };
            Inst::VDupElement {
                vd: VReg(bits(raw, 4, 0) as u8),
                vn: VReg(bits(raw, 9, 5) as u8),
                lane_bytes,
                index,
                q,
                scalar,
            }
        }
        _ => Inst::Unknown { raw },
    }
}

/// The three-different group, whose results are twice as wide as its inputs.
///
/// `Q` here does not select the register width -- the destination is always
/// 128 bits -- but which half of the narrow sources is read, which is the `2`
/// suffix in the assembly.
fn decode_simd_three_different(raw: u32, q: bool, u: u32) -> Option<Inst> {
    let op = match bits(raw, 15, 12) {
        0b0000 => VWidenOp::AddL,
        0b0001 => VWidenOp::AddW,
        0b0010 => VWidenOp::SubL,
        0b0011 => VWidenOp::SubW,
        0b0101 => VWidenOp::AbaL,
        0b0111 => VWidenOp::AbdL,
        0b1000 => VWidenOp::MlaL,
        0b1010 => VWidenOp::MlsL,
        0b1100 => VWidenOp::MulL,
        // The narrowing forms: two vectors of one width giving one of half
        // that width, keeping the top halves of the results. glibc's `strlen`
        // folds a byte mask to a scalar with `addhn`, so a dynamic glibc
        // program reaches this before it reaches `main`.
        0b0100 | 0b0110 => {
            let narrow = match (bits(raw, 15, 12), u) {
                (0b0100, 0) => VNarrowOp::AddHn,
                (0b0100, _) => VNarrowOp::RAddHn,
                (0b0110, 0) => VNarrowOp::SubHn,
                _ => VNarrowOp::RSubHn,
            };
            // The sources are one size up from the result, so a 64-bit
            // result lane -- `size` of 0b11 -- would need 128-bit sources.
            let dst_bytes = match bits(raw, 23, 22) {
                0b00 => 1,
                0b01 => 2,
                0b10 => 4,
                _ => return None,
            };
            return Some(Inst::VNarrow {
                op: narrow,
                dst_bytes,
                // `Q` names which half of the destination is written, not the
                // width of anything: `addhn2` fills the top and leaves the
                // bottom as it was.
                upper: q,
                vd: VReg(bits(raw, 4, 0) as u8),
                vn: VReg(bits(raw, 9, 5) as u8),
                vm: VReg(bits(raw, 20, 16) as u8),
            });
        }
        // The saturating-doubling and polynomial forms share this space but
        // do something else entirely.
        _ => return None,
    };
    // A 64-bit source lane would give a 128-bit result, which does not exist.
    let lane_bytes = match bits(raw, 23, 22) {
        0b00 => 1,
        0b01 => 2,
        0b10 => 4,
        _ => return None,
    };
    Some(Inst::VWiden {
        op,
        arr: VArr {
            lane_bytes,
            q,
            float: false,
            scalar: false,
        },
        upper: q,
        signed: u == 0,
        vd: VReg(bits(raw, 4, 0) as u8),
        vn: VReg(bits(raw, 9, 5) as u8),
        vm: VReg(bits(raw, 20, 16) as u8),
    })
}

fn decode_simd_three_same(raw: u32, q: bool, u: u32) -> Inst {
    let size = bits(raw, 23, 22);
    let opcode = bits(raw, 15, 11);
    let vd = VReg(bits(raw, 4, 0) as u8);
    let vn = VReg(bits(raw, 9, 5) as u8);
    let vm = VReg(bits(raw, 20, 16) as u8);

    // The bitwise subgroup keeps its own variants: `mov vd, vn` is spelled
    // `orr vd, vn, vn`, and the selects take three operands.
    if opcode == 0b00011 {
        if u == 0 {
            let op = match size {
                0b00 => VLogOp::And,
                0b01 => VLogOp::Bic,
                0b10 => VLogOp::Orr,
                _ => VLogOp::Orn,
            };
            return Inst::VLogical { op, vd, vn, vm, q };
        }
        return match size {
            0b00 => Inst::VLogical {
                op: VLogOp::Eor,
                vd,
                vn,
                vm,
                q,
            },
            0b01 => Inst::VSelect {
                kind: VSelKind::Bsl,
                vd,
                vn,
                vm,
                q,
            },
            0b10 => Inst::VSelect {
                kind: VSelKind::Bit,
                vd,
                vn,
                vm,
                q,
            },
            _ => Inst::VSelect {
                kind: VSelKind::Bif,
                vd,
                vn,
                vm,
                q,
            },
        };
    }

    // Floating point: opcode 11xxx, bit 23 selects between the paired
    // operations and bit 22 the precision.
    if opcode >= 0b11000 {
        let lane_bytes = if bit(raw, 22) == 1 { 8 } else { 4 };
        let hi = bit(raw, 23) == 1;
        let op = match (u, opcode, hi) {
            (0, 0b11000, false) => VOp3::FMaxNm,
            (0, 0b11000, true) => VOp3::FMinNm,
            (0, 0b11001, false) => VOp3::FMla,
            (0, 0b11001, true) => VOp3::FMls,
            (0, 0b11010, false) => VOp3::FAdd,
            (0, 0b11010, true) => VOp3::FSub,
            (0, 0b11100, false) => VOp3::FCmEq,
            (0, 0b11110, false) => VOp3::FMax,
            (0, 0b11110, true) => VOp3::FMin,
            (1, 0b11010, false) => VOp3::FAddP,
            (1, 0b11011, false) => VOp3::FMul,
            (1, 0b11100, false) => VOp3::FCmGe,
            (1, 0b11100, true) => VOp3::FCmGt,
            (1, 0b11111, false) => VOp3::FDiv,
            _ => return Inst::Unknown { raw },
        };
        return Inst::VThreeSame {
            op,
            arr: VArr {
                lane_bytes,
                q,
                float: true,
                scalar: false,
            },
            vd,
            vn,
            vm,
        };
    }

    let Some(lane_bytes) = lane_from_size(size) else {
        return Inst::Undefined { raw };
    };
    let op = match (opcode, u) {
        (0b00001, 0) => VOp3::SqAdd,
        (0b00001, _) => VOp3::UqAdd,
        (0b00010, 1) => VOp3::UrHAdd,
        (0b00101, 0) => VOp3::SqSub,
        (0b00101, _) => VOp3::UqSub,
        (0b00110, 0) => VOp3::CmGt,
        (0b00110, _) => VOp3::CmHi,
        (0b00111, 0) => VOp3::CmGe,
        (0b00111, _) => VOp3::CmHs,
        (0b01100, 0) => VOp3::SMax,
        (0b01100, _) => VOp3::UMax,
        (0b01101, 0) => VOp3::SMin,
        (0b01101, _) => VOp3::UMin,
        (0b01110, 0) => VOp3::SAbd,
        (0b01110, _) => VOp3::UAbd,
        (0b10000, 0) => VOp3::Add,
        (0b10000, _) => VOp3::Sub,
        (0b10001, 0) => VOp3::CmTst,
        (0b10001, _) => VOp3::CmEq,
        (0b10010, 0) => VOp3::Mla,
        (0b10010, _) => VOp3::Mls,
        (0b10011, 0) => VOp3::Mul,
        (0b10111, 0) => VOp3::AddP,
        (0b01000, 0) => VOp3::SShl,
        (0b01000, _) => VOp3::UShl,
        (0b10100, 0) => VOp3::SMaxP,
        (0b10100, _) => VOp3::UMaxP,
        (0b10101, 0) => VOp3::SMinP,
        (0b10101, _) => VOp3::UMinP,
        _ => return Inst::Unknown { raw },
    };
    // Doubleword lanes exist only for add, sub, the comparisons and the
    // saturating forms.
    if lane_bytes == 8 && matches!(op, VOp3::Mul | VOp3::Mla | VOp3::Mls) {
        return Inst::Undefined { raw };
    }
    Inst::VThreeSame {
        op,
        arr: VArr {
            lane_bytes,
            q,
            float: false,
            scalar: false,
        },
        vd,
        vn,
        vm,
    }
}

fn decode_simd_two_misc(raw: u32, q: bool, u: u32) -> Inst {
    let size = bits(raw, 23, 22);
    let opcode = bits(raw, 16, 12);
    let vd = VReg(bits(raw, 4, 0) as u8);
    let vn = VReg(bits(raw, 9, 5) as u8);

    // Bit 23 belongs to `size` for the integer members of this group, so the
    // floating-point half has to be picked out by opcode: everything at or
    // above 0b11000, plus the compare-with-zero and sign block when bit 23
    // marks it as the second size pair. `rev64 v0.4s` has size = 10 and would
    // be misread as floating point by a bit-23 test alone.
    // `fcvtl`/`fcvtn` change the lane width, so they cannot share the
    // same-shape lowering the rest of this group uses.
    if u == 0 && matches!(opcode, 0b10110 | 0b10111) {
        return Inst::VFpCvtPrecision {
            widen: opcode == 0b10111,
            narrow_bytes: if bit(raw, 22) == 1 { 4 } else { 2 },
            upper: q,
            vd,
            vn,
        };
    }

    let fp_high = opcode >= 0b11000;
    let fp_low = bit(raw, 23) == 1 && (0b01100..=0b01111).contains(&opcode);
    if fp_high || fp_low {
        let lane_bytes = if bit(raw, 22) == 1 { 8 } else { 4 };
        let hi = bit(raw, 23) == 1;
        let op = match (opcode, u, hi) {
            (0b01100, 0, _) => VOp2::FCmGtZero,
            (0b01101, 0, _) => VOp2::FCmEqZero,
            (0b01110, 0, _) => VOp2::FCmLtZero,
            (0b01111, 0, _) => VOp2::FAbs,
            (0b01100, _, _) => VOp2::FCmGeZero,
            (0b01101, _, _) => VOp2::FCmLeZero,
            (0b01111, _, _) => VOp2::FNeg,
            (0b11000, 0, false) => VOp2::FrintN,
            (0b11000, 0, true) => VOp2::FrintP,
            (0b11000, _, false) => VOp2::FrintA,
            (0b11001, 0, false) => VOp2::FrintM,
            (0b11001, 0, true) => VOp2::FrintZ,
            (0b11001, _, false) => VOp2::FrintX,
            (0b11001, _, true) => VOp2::FrintI,
            (0b11010, 0, true) => VOp2::FcvtPs,
            (0b11010, _, true) => VOp2::FcvtPu,
            (0b11010, 0, false) => VOp2::FcvtNs,
            (0b11010, _, false) => VOp2::FcvtNu,
            (0b11011, 0, true) => VOp2::FcvtZs,
            (0b11011, _, true) => VOp2::FcvtZu,
            (0b11011, 0, false) => VOp2::FcvtMs,
            (0b11011, _, false) => VOp2::FcvtMu,
            (0b11100, 0, false) => VOp2::FcvtAs,
            (0b11100, _, false) => VOp2::FcvtAu,
            (0b11101, 0, false) => VOp2::Scvtf,
            (0b11101, _, false) => VOp2::Ucvtf,
            (0b11111, _, true) => VOp2::FSqrt,
            _ => return Inst::Unknown { raw },
        };
        return Inst::VTwoMisc {
            op,
            arr: VArr {
                lane_bytes,
                q,
                float: true,
                scalar: false,
            },
            vd,
            vn,
        };
    }

    let Some(lane_bytes) = lane_from_size(size) else {
        return Inst::Undefined { raw };
    };
    let op = match (opcode, u) {
        (0b00000, 0) => VOp2::Rev64,
        (0b00000, _) => VOp2::Rev32,
        (0b00001, 0) => VOp2::Rev16,
        (0b00100, 0) => VOp2::Cls,
        (0b00100, _) => VOp2::Clz,
        (0b00101, 0) => VOp2::Cnt,
        (0b00101, _) if size == 0b00 => VOp2::Not,
        (0b00101, _) => VOp2::Rbit,
        (0b01000, 0) => VOp2::CmGtZero,
        (0b01000, _) => VOp2::CmGeZero,
        (0b01001, 0) => VOp2::CmEqZero,
        (0b01001, _) => VOp2::CmLeZero,
        (0b01010, 0) => VOp2::CmLtZero,
        (0b01011, 0) => VOp2::Abs,
        (0b01011, _) => VOp2::Neg,
        (0b10010, 0) => VOp2::Xtn,
        (0b10100, 0) => VOp2::SqXtn,
        (0b10100, _) => VOp2::UqXtn,
        _ => return Inst::Unknown { raw },
    };
    Inst::VTwoMisc {
        op,
        arr: VArr {
            lane_bytes,
            q,
            float: false,
            scalar: false,
        },
        vd,
        vn,
    }
}

fn decode_simd_across(raw: u32, q: bool, u: u32) -> Inst {
    let opcode = bits(raw, 16, 12);
    let vd = VReg(bits(raw, 4, 0) as u8);
    let vn = VReg(bits(raw, 9, 5) as u8);

    // The floating-point reductions put the precision in bit 22 and use
    // bit 23 to pick between the max and min of each pair.
    if u == 1 && matches!(opcode, 0b01100 | 0b01111) {
        let op = match (opcode, bit(raw, 23)) {
            (0b01111, 0) => VAcrossOp::FMax,
            (0b01111, _) => VAcrossOp::FMin,
            (_, 0) => VAcrossOp::FMaxNm,
            (_, _) => VAcrossOp::FMinNm,
        };
        return Inst::VAcross {
            op,
            arr: VArr {
                lane_bytes: 4,
                q,
                float: true,
                scalar: false,
            },
            vd,
            vn,
        };
    }

    let op = match (opcode, u) {
        (0b01010, 0) => VAcrossOp::SMax,
        (0b01010, _) => VAcrossOp::UMax,
        (0b11010, 0) => VAcrossOp::SMin,
        (0b11010, _) => VAcrossOp::UMin,
        (0b11011, 0) => VAcrossOp::Add,
        _ => return Inst::Unknown { raw },
    };
    let lane_bytes = match bits(raw, 23, 22) {
        0b00 => 1,
        0b01 => 2,
        0b10 => 4,
        _ => return Inst::Undefined { raw },
    };
    // A word-sized lane leaves only two of them in the 64-bit form, which
    // the architecture does not define for these reductions.
    if lane_bytes == 4 && !q {
        return Inst::Undefined { raw };
    }
    Inst::VAcross {
        op,
        arr: VArr {
            lane_bytes,
            q,
            float: false,
            scalar: false,
        },
        vd,
        vn,
    }
}

fn decode_simd_shift_imm(raw: u32, q: bool, u: u32, immh: u32, immb: u32) -> Inst {
    let opcode = bits(raw, 15, 11);
    let vd = VReg(bits(raw, 4, 0) as u8);
    let vn = VReg(bits(raw, 9, 5) as u8);
    // `immh`'s highest set bit gives the element width; the rest of it joins
    // `immb` to form the amount.
    let (lane_bytes, esize) = if immh & 0b1000 != 0 {
        (8u32, 64u32)
    } else if immh & 0b0100 != 0 {
        (4, 32)
    } else if immh & 0b0010 != 0 {
        (2, 16)
    } else {
        (1, 8)
    };
    let imm = (immh << 3) | immb;
    let op = match (opcode, u) {
        (0b01010, 0) => VShiftImmOp::Shl,
        (0b00000, 0) => VShiftImmOp::SShr,
        (0b00000, _) => VShiftImmOp::UShr,
        (0b10100, 0) => VShiftImmOp::SShll,
        (0b10100, _) => VShiftImmOp::UShll,
        (0b10000, 0) => VShiftImmOp::Shrn,
        _ => return Inst::Unknown { raw },
    };
    // Left shifts count up from the element size; right shifts count down.
    let left = matches!(
        op,
        VShiftImmOp::Shl | VShiftImmOp::SShll | VShiftImmOp::UShll
    );
    let shift = if left {
        imm.wrapping_sub(esize)
    } else {
        (2 * esize).wrapping_sub(imm)
    };
    // A left shift can move by at most one less than the element width; a
    // right shift may move by the whole of it, which for the narrowing forms
    // is the common case -- `shrn v0.2s, v0.2d, #32` takes the top half of
    // each doubleword, and rejecting it made apt's hash table unreachable.
    if (left && shift >= esize) || (!left && shift > esize) {
        return Inst::Undefined { raw };
    }
    Inst::VShiftImm {
        op,
        arr: VArr {
            lane_bytes,
            q,
            float: false,
            scalar: false,
        },
        vd,
        vn,
        shift,
    }
}

/// Scalar floating-point.
///
/// Only single and double precision are decoded; half precision needs
/// FEAT_FP16, which the guest is told it does not have. Discrimination
/// follows the Arm ARM tables C4-25 onwards: the `11111` sub-group is
/// multiply-add, and within `11110` bits `[11:10]` choose the family, with
/// integer conversion recognised first because it is the only member whose
/// bit 31 is `sf` rather than a fixed zero.
/// The by-element multiply family.
///
/// The lane index is spread across three fields and assembled differently for
/// each element width, because a 16-bit arrangement has eight lanes to address
/// and only five register bits to spare.
fn decode_simd_by_element(raw: u32, q: bool, u: u32) -> Option<Inst> {
    let opcode = bits(raw, 15, 12);
    let float = matches!(opcode, 0b0001 | 0b0101 | 0b1001);
    let op = match (u, opcode) {
        (0, 0b0001) => VOp3::FMla,
        (0, 0b0101) => VOp3::FMls,
        (0, 0b1001) => VOp3::FMul,
        (1, 0b1001) => VOp3::FMulx,
        (0, 0b1000) => VOp3::Mul,
        (1, 0b0000) => VOp3::Mla,
        (1, 0b0100) => VOp3::Mls,
        _ => return None,
    };
    let size = bits(raw, 23, 22);
    let m = bit(raw, 20);
    let h = bit(raw, 11);
    let l = bit(raw, 21);
    // Half precision has no wasm lane type, so it stays undecoded.
    let (lane_bytes, index, vm) = match (size, float) {
        (0b01, false) => (2, h << 2 | l << 1 | m, bits(raw, 19, 16)),
        (0b10, _) => (4, h << 1 | l, m << 4 | bits(raw, 19, 16)),
        (0b11, true) => (8, h, m << 4 | bits(raw, 19, 16)),
        _ => return None,
    };
    Some(Inst::VByElement {
        op,
        arr: VArr {
            lane_bytes,
            q,
            float,
            scalar: false,
        },
        vd: VReg(bits(raw, 4, 0) as u8),
        vn: VReg(bits(raw, 9, 5) as u8),
        vm: VReg(vm as u8),
        index,
    })
}

fn decode_scalar_fp(raw: u32) -> Inst {
    let group = bits(raw, 28, 24);
    if group == 0b11111 {
        return decode_fp_madd(raw);
    }
    if group != 0b11110 || bit(raw, 29) != 0 || bit(raw, 30) != 0 {
        return Inst::Unknown { raw };
    }
    if bit(raw, 21) == 0 {
        return decode_fp_fixed_convert(raw, sz_of(bits(raw, 23, 22)));
    }
    // `fmov` to and from the upper half of a 128-bit register is the one
    // member of this family with ptype = 10, because the operand really is
    // 128 bits wide.
    if bits(raw, 15, 10) == 0
        && bits(raw, 23, 22) == 0b10
        && bits(raw, 20, 19) == 0b01
        && bit(raw, 31) == 1
        && bits(raw, 18, 17) == 0b11
    {
        return Inst::FpMoveTopHalf {
            to_vector: bit(raw, 16) == 1,
            rd: RegSel::gp(bits(
                raw,
                if bit(raw, 16) == 1 { 9 } else { 4 },
                if bit(raw, 16) == 1 { 5 } else { 0 },
            )),
            vd: VReg(bits(
                raw,
                if bit(raw, 16) == 1 { 4 } else { 9 },
                if bit(raw, 16) == 1 { 0 } else { 5 },
            ) as u8),
        };
    }
    let Some(sz) = fp_size(bits(raw, 23, 22)) else {
        return Inst::Unknown { raw };
    };
    let vd = VReg(bits(raw, 4, 0) as u8);
    let vn = VReg(bits(raw, 9, 5) as u8);
    let vm = VReg(bits(raw, 20, 16) as u8);

    // Conversion between floating point and integer.
    if bits(raw, 15, 10) == 0 {
        return decode_fp_int_convert(raw, sz);
    }
    // Everything below requires M == 0.
    if bit(raw, 31) != 0 {
        return Inst::Unknown { raw };
    }

    match bits(raw, 11, 10) {
        0b00 => {
            if bit(raw, 12) == 1 {
                // Floating-point immediate: imm8 in bits 20:13.
                if bits(raw, 9, 5) != 0 {
                    return Inst::Undefined { raw };
                }
                let imm8 = bits(raw, 20, 13);
                return Inst::FpMoveImm {
                    sz,
                    vd,
                    bits: expand_fp_immediate(imm8, sz),
                };
            }
            if bit(raw, 13) == 1 {
                // Floating-point compare. Bit 3 of opcode2 selects the
                // compare-against-zero form; the `E` variants differ only in
                // exception behaviour, which gex does not model.
                let opcode2 = bits(raw, 4, 0);
                if opcode2 & 0b00111 != 0 {
                    return Inst::Unknown { raw };
                }
                let against_zero = opcode2 & 0b01000 != 0;
                return Inst::FpCompare {
                    sz,
                    vn,
                    vm: if against_zero { None } else { Some(vm) },
                };
            }
            if bit(raw, 14) == 1 {
                return decode_fp_one_source(raw, sz, vd, vn);
            }
            Inst::Unknown { raw }
        }
        // Floating-point conditional compare. The `E` variant differs only in
        // which NaNs raise, which gex does not model, so both decode alike.
        0b01 => Inst::FpCondCompare {
            sz,
            vn,
            vm,
            nzcv: bits(raw, 3, 0) as u8,
            cond: Cond(bits(raw, 15, 12) as u8),
            signalling: bit(raw, 4) == 1,
        },
        0b10 => {
            let op = match bits(raw, 15, 12) {
                0b0000 => FpOp2::Mul,
                0b0001 => FpOp2::Div,
                0b0010 => FpOp2::Add,
                0b0011 => FpOp2::Sub,
                0b0100 => FpOp2::Max,
                0b0101 => FpOp2::Min,
                0b0110 => FpOp2::MaxNum,
                0b0111 => FpOp2::MinNum,
                0b1000 => FpOp2::NMul,
                _ => return Inst::Unknown { raw },
            };
            Inst::FpDataProc2 { sz, op, vd, vn, vm }
        }
        _ => Inst::FpCondSelect {
            sz,
            vd,
            vn,
            vm,
            cond: Cond(bits(raw, 15, 12) as u8),
        },
    }
}

/// Decode an `imm5` lane specifier into `(lane size in bytes, lane index)`.
///
/// The lowest set bit selects the element width and the bits above it are
/// the index, so a wider element simply consumes more of the field.
fn decode_lane(imm5: u32) -> Option<(u32, u32)> {
    if imm5 & 1 != 0 {
        Some((1, (imm5 >> 1) & 0xf))
    } else if imm5 & 2 != 0 {
        Some((2, (imm5 >> 2) & 0x7))
    } else if imm5 & 4 != 0 {
        Some((4, (imm5 >> 3) & 0x3))
    } else if imm5 & 8 != 0 {
        Some((8, (imm5 >> 4) & 0x1))
    } else {
        None
    }
}

fn fp_size(ptype: u32) -> Option<FpSz> {
    match ptype {
        0b00 => Some(FpSz::S),
        0b01 => Some(FpSz::D),
        _ => None,
    }
}

fn decode_fp_one_source(raw: u32, sz: FpSz, vd: VReg, vn: VReg) -> Inst {
    let opcode = bits(raw, 20, 15);
    // `fcvt` between precisions shares this group.
    if opcode >> 2 == 0b0001 {
        let Some(to) = fp_size(opcode & 0b11) else {
            return Inst::Unknown { raw };
        };
        if to == sz {
            return Inst::Undefined { raw };
        }
        return Inst::FpConvertPrecision {
            from: sz,
            to,
            vd,
            vn,
        };
    }
    let op = match opcode {
        0b000000 => FpOp1::Mov,
        0b000001 => FpOp1::Abs,
        0b000010 => FpOp1::Neg,
        0b000011 => FpOp1::Sqrt,
        0b001000 => FpOp1::RoundNearest,
        0b001001 => FpOp1::RoundPlus,
        0b001010 => FpOp1::RoundMinus,
        0b001011 => FpOp1::RoundZero,
        0b001100 => FpOp1::RoundAway,
        // FRINTX and FRINTI honour the FPCR rounding mode and FRINTX also
        // reports inexact. gex tracks neither in fast mode (PLAN §5.7), so
        // both behave as round-to-nearest — but they keep their own
        // identities so the disassembler stays faithful.
        0b001110 => FpOp1::RoundExact,
        0b001111 => FpOp1::RoundCurrent,
        _ => return Inst::Unknown { raw },
    };
    Inst::FpDataProc1 { sz, op, vd, vn }
}

fn decode_fp_madd(raw: u32) -> Inst {
    if bit(raw, 31) != 0 || bit(raw, 30) != 0 || bit(raw, 29) != 0 {
        return Inst::Unknown { raw };
    }
    let Some(sz) = fp_size(bits(raw, 23, 22)) else {
        return Inst::Unknown { raw };
    };
    let o1 = bit(raw, 21);
    let o0 = bit(raw, 15);
    // FMADD: +(n*m) + a   FMSUB: -(n*m) + a
    // FNMADD: -(n*m) - a  FNMSUB: +(n*m) - a
    let (neg_product, neg_addend) = match (o1, o0) {
        (0, 0) => (false, false),
        (0, 1) => (true, false),
        (1, 0) => (true, true),
        _ => (false, true),
    };
    Inst::FpDataProc3 {
        sz,
        vd: VReg(bits(raw, 4, 0) as u8),
        vn: VReg(bits(raw, 9, 5) as u8),
        vm: VReg(bits(raw, 20, 16) as u8),
        va: VReg(bits(raw, 14, 10) as u8),
        neg_product,
        neg_addend,
    }
}

/// Conversion between a floating-point register and a *fixed-point* integer.
///
/// The same opcodes as the integer forms with bit 21 clear, plus a scale
/// field: the integer is read (or written) as a value with `64 - scale`
/// fractional bits, so `scvtf d0, w0, #2` divides by four.
fn decode_fp_fixed_convert(raw: u32, sz: Option<FpSz>) -> Inst {
    let Some(sz) = sz else {
        return Inst::Unknown { raw };
    };
    let sf = bit(raw, 31);
    let int_sz = Sz::from_sf(sf);
    let scale = bits(raw, 15, 10);
    let fbits = 64 - scale;
    // A 32-bit register cannot carry more than 32 fractional bits, and zero
    // fractional bits is the non-fixed-point encoding, which is elsewhere.
    if fbits == 0 || fbits > 64 || (sf == 0 && fbits > 32) {
        return Inst::Undefined { raw };
    }
    let rd = RegSel::gp(bits(raw, 4, 0));
    let rn = RegSel::gp(bits(raw, 9, 5));
    let vd = VReg(bits(raw, 4, 0) as u8);
    let vn = VReg(bits(raw, 9, 5) as u8);
    match (bits(raw, 20, 19), bits(raw, 18, 16)) {
        // Fixed point to floating point.
        (0b00, o @ (0b010 | 0b011)) => Inst::FpFromInt {
            sz,
            int_sz,
            signed: o == 0b010,
            vd,
            rn,
            fbits,
        },
        // Floating point to fixed point, always rounding toward zero.
        (0b11, o @ (0b000 | 0b001)) => Inst::FpToInt {
            sz,
            int_sz,
            signed: o == 0b000,
            round: FpRound::Zero,
            rd,
            vn,
            fbits,
        },
        _ => Inst::Unknown { raw },
    }
}

/// The `type` field's floating-point size, or `None` for the reserved values.
fn sz_of(ty: u32) -> Option<FpSz> {
    fp_size(ty)
}

fn decode_fp_int_convert(raw: u32, sz: FpSz) -> Inst {
    let sf = bit(raw, 31);
    let int_sz = Sz::from_sf(sf);
    let rmode = bits(raw, 20, 19);
    let opcode = bits(raw, 18, 16);
    let rd = RegSel::gp(bits(raw, 4, 0));
    let rn = RegSel::gp(bits(raw, 9, 5));
    let vd = VReg(bits(raw, 4, 0) as u8);
    let vn = VReg(bits(raw, 9, 5) as u8);

    // Rounding mode is spread across `rmode` and the low opcode bit for the
    // "round to nearest, ties away" forms.
    let round = match (rmode, opcode) {
        (0b00, 0b000 | 0b001) => Some(FpRound::Nearest),
        (0b00, 0b100 | 0b101) => Some(FpRound::Away),
        (0b01, 0b000 | 0b001) => Some(FpRound::Plus),
        (0b10, 0b000 | 0b001) => Some(FpRound::Minus),
        (0b11, 0b000 | 0b001) => Some(FpRound::Zero),
        _ => None,
    };
    if let Some(round) = round {
        return Inst::FpToInt {
            sz,
            int_sz,
            signed: opcode & 1 == 0,
            round,
            rd,
            vn,
            fbits: 0,
        };
    }
    match (rmode, opcode) {
        (0b00, 0b010) => Inst::FpFromInt {
            sz,
            int_sz,
            signed: true,
            vd,
            rn,
            fbits: 0,
        },
        (0b00, 0b011) => Inst::FpFromInt {
            sz,
            int_sz,
            signed: false,
            vd,
            rn,
            fbits: 0,
        },
        (0b00, 0b110) => Inst::FpMoveGeneral {
            sz,
            to_vector: false,
            rd,
            vd: vn,
        },
        (0b00, 0b111) => Inst::FpMoveGeneral {
            sz,
            to_vector: true,
            rd: rn,
            vd,
        },
        // rmode=01 with opcode 110/111 moves the *top* half of a 128-bit
        // register, which needs the full vector view.
        _ => Inst::Unknown { raw },
    }
}

/// `VFPExpandImm`: turn an 8-bit `fmov` immediate into its bit pattern.
fn expand_fp_immediate(imm8: u32, sz: FpSz) -> u64 {
    let a = ((imm8 >> 7) & 1) as u64;
    let b = ((imm8 >> 6) & 1) as u64;
    let cdefgh = (imm8 & 0x3f) as u64;
    match sz {
        FpSz::S => {
            // sign : NOT(b) : b*5 : cdefgh : 0*19
            let mut v = a << 31;
            v |= (1 - b) << 30;
            let rep = if b != 0 { 0b11111u64 } else { 0 };
            v |= rep << 25;
            v |= cdefgh << 19;
            v
        }
        FpSz::D => {
            // sign : NOT(b) : b*8 : cdefgh : 0*48
            let mut v = a << 63;
            v |= (1 - b) << 62;
            let rep = if b != 0 { 0xffu64 } else { 0 };
            v |= rep << 54;
            v |= cdefgh << 48;
            v
        }
    }
}

/// `AdvSIMDExpandImm`: turn `imm8` and `cmode` into the full register value.
///
/// Returns `None` for the encodings gex does not model yet (the
/// floating-point forms), which keeps the mapping honest rather than
/// approximate.
fn expand_simd_immediate(imm8: u32, cmode: u32, op: bool, q: bool) -> Option<u128> {
    let imm8 = imm8 as u64;
    let lane: u64;
    let lane_bits: u32;
    match (cmode >> 1, cmode & 1) {
        // cmode = 0xx0: 32-bit lanes, imm8 shifted by 0/8/16/24.
        (c, 0) if c < 4 => {
            lane = imm8 << (8 * c);
            lane_bits = 32;
        }
        // cmode = 10x0: 16-bit lanes, imm8 shifted by 0/8.
        (c, 0) if (4..6).contains(&c) => {
            lane = imm8 << (8 * (c - 4));
            lane_bits = 16;
        }
        // cmode = 110x: 32-bit lanes, imm8 shifted up with ones below.
        (6, _) => {
            let shift = 8 * (1 + (cmode & 1));
            lane = (imm8 << shift) | ((1u64 << shift) - 1);
            lane_bits = 32;
        }
        (7, 0) if !op => {
            // cmode = 1110, op = 0: replicate imm8 into every byte.
            lane = imm8;
            lane_bits = 8;
        }
        (7, 0) if op => {
            // cmode = 1110, op = 1: each bit of imm8 expands to a byte.
            let mut v = 0u64;
            for i in 0..8 {
                if imm8 & (1 << i) != 0 {
                    v |= 0xffu64 << (i * 8);
                }
            }
            // This form is always 64-bit lanes and never inverted.
            let half = v as u128;
            return Some(if q { half | (half << 64) } else { half });
        }
        // cmode = 1111: a floating-point immediate, expanded exactly as the
        // scalar `fmov` form does.
        (7, 1) => {
            let bits = expand_fp_immediate(imm8 as u32, if op { FpSz::D } else { FpSz::S });
            return Some(if op {
                // The 64-bit form needs Q; the assembler will not emit the
                // other combination.
                let half = bits as u128;
                if q {
                    half | (half << 64)
                } else {
                    half
                }
            } else {
                let lane = bits as u128 & 0xffff_ffff;
                let mut v = 0u128;
                let mut shift = 0;
                while shift < 128 {
                    v |= lane << shift;
                    shift += 32;
                }
                if q {
                    v
                } else {
                    v & u64::MAX as u128
                }
            });
        }
        _ => return None,
    }

    // MVNI inverts the lane before replication.
    let lane = if op {
        (!lane) & ((1u64 << lane_bits) - 1)
    } else {
        lane & ((1u64 << lane_bits) - 1)
    };
    let mut v: u128 = 0;
    let mut shift = 0;
    while shift < 128 {
        v |= (lane as u128) << shift;
        shift += lane_bits;
    }
    // The 64-bit form zeroes the upper half of the register.
    Some(if q { v } else { v & u64::MAX as u128 })
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
                imm, rd, set_flags, ..
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
