//! Atomic operations on one word of guest memory.
//!
//! Guest memory is shared with guest threads that run wasm atomics on it
//! (glibc's mutex fast paths are a `lock cmpxchg` in guest code), so a kernel
//! operation that must update a word *against* them (a priority-inheritance
//! futex: `FUTEX_LOCK_PI` sets the waiters bit on a word the owner may be
//! releasing in user space at the same moment) needs a real atomic, not a
//! read and a write. gex-kernel forbids unsafe code; this crate is the one
//! place that views a guest word as an atomic.
//!
//! Every function takes the guest memory as the kernel sees it (a byte
//! slice) and an offset, and fails (`None`) for an offset that is out of
//! range or not 4-aligned, as the futex syscalls require.

pub mod rcu;

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

fn word(bytes: &mut [u8], off: u64) -> Option<&AtomicU32> {
    let off = usize::try_from(off).ok()?;
    if off % 4 != 0 || off.checked_add(4)? > bytes.len() {
        return None;
    }
    let p = bytes[off..off + 4].as_mut_ptr() as *mut u32;
    // SAFETY: `p` is in bounds and 4-aligned (the slice starts at a wasm
    // memory, which is page-aligned, and `off` is a multiple of 4), and the
    // borrow of `bytes` keeps the memory alive for the reference's lifetime.
    // Other threads touch this word only through atomics too: guest code's
    // are wasm atomics on the same shared memory.
    Some(unsafe { AtomicU32::from_ptr(p) })
}

/// An atomic load of an 8-aligned 64-bit word (a seqlock's sequence the
/// guest stores whole).
pub fn load_u64(bytes: &mut [u8], off: u64) -> Option<u64> {
    let off = usize::try_from(off).ok()?;
    if off % 8 != 0 || off.checked_add(8)? > bytes.len() {
        return None;
    }
    let p = bytes[off..off + 8].as_mut_ptr() as *mut u64;
    // SAFETY: as `word`'s, for an 8-aligned word (a wasm memory starts
    // page-aligned).
    Some(unsafe { AtomicU64::from_ptr(p) }.load(Ordering::SeqCst))
}

/// Compare-and-swap: `Ok(old)` when the word held `expected` and now holds
/// `new`, `Err(actual)` otherwise.
pub fn cas_u32(bytes: &mut [u8], off: u64, expected: u32, new: u32) -> Option<Result<u32, u32>> {
    Some(word(bytes, off)?.compare_exchange(expected, new, Ordering::SeqCst, Ordering::SeqCst))
}

/// An atomic load.
pub fn load_u32(bytes: &mut [u8], off: u64) -> Option<u32> {
    Some(word(bytes, off)?.load(Ordering::SeqCst))
}

/// An atomic store (release, and every earlier store of this thread's with
/// it: a lock word's release).
pub fn store_u32(bytes: &mut [u8], off: u64, v: u32) -> Option<()> {
    word(bytes, off)?.store(v, Ordering::SeqCst);
    Some(())
}

/// An atomic exchange: the old value.
pub fn swap_u32(bytes: &mut [u8], off: u64, v: u32) -> Option<u32> {
    Some(word(bytes, off)?.swap(v, Ordering::SeqCst))
}

/// An atomic add: the old value.
pub fn add_u32(bytes: &mut [u8], off: u64, v: u32) -> Option<u32> {
    Some(word(bytes, off)?.fetch_add(v, Ordering::SeqCst))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cas_succeeds_and_fails_as_compare_exchange_does() {
        let mut m = vec![0u8; 16];
        assert_eq!(cas_u32(&mut m, 4, 0, 7), Some(Ok(0)));
        assert_eq!(cas_u32(&mut m, 4, 0, 9), Some(Err(7)));
        assert_eq!(load_u32(&mut m, 4), Some(7));
        assert_eq!(cas_u32(&mut m, 2, 0, 1), None);
        assert_eq!(cas_u32(&mut m, 16, 0, 1), None);
    }
}
