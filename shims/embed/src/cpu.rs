//! Reading and writing a guest context (`gex_abi::ctx`) in linear memory.

use gex_abi::ctx;

#[inline]
fn at(c: u32, off: usize) -> *mut u8 {
    (c as usize + off) as *mut u8
}

#[inline]
pub fn get_u64(c: u32, off: usize) -> u64 {
    // SAFETY: `c` is a live context and `off` inside it.
    unsafe { (at(c, off) as *const u64).read_unaligned() }
}

#[inline]
pub fn set_u64(c: u32, off: usize, v: u64) {
    // SAFETY: as above.
    unsafe { (at(c, off) as *mut u64).write_unaligned(v) }
}

#[inline]
pub fn set_u32(c: u32, off: usize, v: u32) {
    // SAFETY: as above.
    unsafe { (at(c, off) as *mut u32).write_unaligned(v) }
}

/// `x<n>` (31 is SP).
#[inline]
pub fn x(c: u32, n: u8) -> u64 {
    get_u64(c, ctx::x(n))
}

#[inline]
pub fn set_x(c: u32, n: u8, v: u64) {
    set_u64(c, ctx::x(n), v)
}

/// The low and high halves of `v<n>`.
#[inline]
pub fn v(c: u32, n: u8) -> [u64; 2] {
    [get_u64(c, ctx::v(n)), get_u64(c, ctx::v(n) + 8)]
}

#[inline]
pub fn set_v(c: u32, n: u8, q: [u64; 2]) {
    set_u64(c, ctx::v(n), q[0]);
    set_u64(c, ctx::v(n) + 8, q[1]);
}

/// A copy of the whole context.
pub fn save(c: u32) -> Box<[u8]> {
    // SAFETY: a live context of `ctx::SIZE` bytes.
    unsafe { core::slice::from_raw_parts(at(c, 0), ctx::SIZE) }.into()
}

/// Put back what [`save`] took.
pub fn restore(c: u32, saved: &[u8]) {
    // SAFETY: as above.
    unsafe { core::ptr::copy_nonoverlapping(saved.as_ptr(), at(c, 0), ctx::SIZE) }
}
