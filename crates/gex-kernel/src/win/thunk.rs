//! Window-procedure thunks an i386 program makes at run time, in the two
//! documented shapes ATL and MFC write (`mov dword [esp+4], imm32; jmp
//! rel32`, and `mov ecx, imm32; jmp rel32`): a run without a translator
//! executes these itself, as each only stores one value and jumps. Any
//! other bytes are not a thunk, and the run stops at them as at any code
//! made at run time.

use crate::GuestMem;

/// The store a thunk makes before its jump.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Store {
    /// `mov dword [esp+4], imm32`: the first stack argument.
    StackArg(u32),
    /// `mov ecx, imm32`.
    Ecx(u32),
}

/// The longest shape, in bytes.
pub const MAX_LEN: usize = 13;

/// The thunk `bytes` (the code at `pc`, as much as is mapped) are, if any:
/// its store and the address it jumps to.
pub fn recognize(bytes: &[u8], pc: u32) -> Option<(Store, u32)> {
    let imm = |o: usize| u32::from_le_bytes(bytes[o..o + 4].try_into().unwrap());
    if bytes.len() >= 13 && bytes[..4] == [0xc7, 0x44, 0x24, 0x04] && bytes[8] == 0xe9 {
        return Some((Store::StackArg(imm(4)), pc.wrapping_add(13).wrapping_add(imm(9))));
    }
    if bytes.len() >= 10 && bytes[0] == 0xb9 && bytes[5] == 0xe9 {
        return Some((Store::Ecx(imm(1)), pc.wrapping_add(10).wrapping_add(imm(6))));
    }
    None
}

/// Run the thunk at `pc`, if one is there, on the i386 guest state at
/// `ctx` (`gex_abi::ctx`'s layout): its store done, the address it jumps
/// to. Memory or state it cannot reach is no thunk.
pub fn run(gm: &mut GuestMem, ctx: u64, pc: u64) -> Option<u64> {
    let n = (0..=MAX_LEN).rev().find(|&n| gm.read(pc, n).is_ok())?;
    let bytes = gm.read(pc, n).ok()?.to_vec();
    let (store, to) = recognize(&bytes, pc as u32)?;
    match store {
        Store::StackArg(v) => {
            let sp = gm.read_u64(ctx + gex_abi::ctx::x(4) as u64).ok()? & 0xffff_ffff;
            gm.write_u32(sp + 4, v).ok()?;
        }
        Store::Ecx(v) => gm.write_u64(ctx + gex_abi::ctx::x(1) as u64, v as u64).ok()?,
    }
    Some(to as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jmp(at: u32, len: u32, to: u32) -> [u8; 4] {
        to.wrapping_sub(at + len).to_le_bytes()
    }

    #[test]
    fn the_stack_argument_shape_stores_and_jumps() {
        let mut b = vec![0xc7, 0x44, 0x24, 0x04, 0x78, 0x56, 0x34, 0x12, 0xe9];
        b.extend(jmp(0x2130fc0, 13, 0x1001000));
        assert_eq!(recognize(&b, 0x2130fc0), Some((Store::StackArg(0x12345678), 0x1001000)));
    }

    #[test]
    fn the_ecx_shape_stores_and_jumps() {
        let mut b = vec![0xb9, 0x78, 0x56, 0x34, 0x12, 0xe9];
        b.extend(jmp(0x2130fc0, 10, 0x1001000));
        assert_eq!(recognize(&b, 0x2130fc0), Some((Store::Ecx(0x12345678), 0x1001000)));
    }

    #[test]
    fn near_misses_are_no_thunk() {
        // Another stack slot, a short jump, an indirect jump, cut short.
        let mut b = vec![0xc7, 0x44, 0x24, 0x08, 0, 0, 0, 0, 0xe9, 0, 0, 0, 0];
        assert_eq!(recognize(&b, 0x1000), None);
        b = vec![0xb9, 0, 0, 0, 0, 0xeb, 0, 0, 0, 0];
        assert_eq!(recognize(&b, 0x1000), None);
        b = vec![0xb9, 0, 0, 0, 0, 0xff, 0x25, 0, 0, 0];
        assert_eq!(recognize(&b, 0x1000), None);
        assert_eq!(recognize(&[0xc7, 0x44, 0x24, 0x04, 0, 0, 0, 0, 0xe9, 0, 0], 0x1000), None);
        assert_eq!(recognize(&[0xc3], 0x1000), None);
    }

    #[test]
    fn running_one_stores_into_the_guest_state() {
        let mut mem = vec![0u8; 0x3000];
        let (ctx, sp, pc) = (0x100u64, 0x2000u64, 0x1000u64);
        mem[ctx as usize + gex_abi::ctx::x(4)..][..8].copy_from_slice(&sp.to_le_bytes());
        let mut t = vec![0xc7, 0x44, 0x24, 0x04, 0x44, 0x33, 0x22, 0x11, 0xe9];
        t.extend(jmp(pc as u32, 13, 0x1800));
        mem[pc as usize..][..13].copy_from_slice(&t);
        let mut gm = GuestMem::new(&mut mem);
        assert_eq!(run(&mut gm, ctx, pc), Some(0x1800));
        assert_eq!(gm.read_u32(sp + 4).unwrap(), 0x11223344);
        let mut e = vec![0xb9, 0x04, 0x03, 0x02, 0x01, 0xe9];
        e.extend(jmp(pc as u32 + 0x40, 10, 0x1900));
        gm.write(pc + 0x40, &e).unwrap();
        assert_eq!(run(&mut gm, ctx, pc + 0x40), Some(0x1900));
        assert_eq!(gm.read_u64(ctx + gex_abi::ctx::x(1) as u64).unwrap(), 0x01020304);
    }
}
