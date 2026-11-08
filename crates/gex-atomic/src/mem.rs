//! Guest memory as a pointer and a length, sliced a range at a time.
//!
//! In the browser the guest's memory is the module's own linear memory from
//! address 0, where no Rust reference may point. A slice of all of it made
//! from a null-based pointer (and a fresh `&mut` of it on every call) was a
//! null reference and aliasing `&mut`, and LLVM folded what that told it:
//! bounds checks to "always out of range", two equal byte ranges to unequal.
//! [`Span`] never makes that slice: each access makes one of only the range
//! it names, and none at address 0. The kernel (which forbids unsafe) holds
//! one in its `GuestMem`.

use std::marker::PhantomData;

/// `len` bytes of memory from `ptr`, or from address 0 of the module's own
/// memory (`ptr` null), borrowed for `'a`.
pub struct Span<'a> {
    ptr: *mut u8,
    len: usize,
    _bytes: PhantomData<&'a mut [u8]>,
}

// The memory is shared with the guest's own threads whatever the borrow says
// (a slice of it was Send and Sync too).
unsafe impl Send for Span<'_> {}
unsafe impl Sync for Span<'_> {}

impl<'a> Span<'a> {
    /// A slice's bytes (a native host's memory, or a buffer).
    pub fn new(bytes: &'a mut [u8]) -> Span<'a> {
        Span { ptr: bytes.as_mut_ptr(), len: bytes.len(), _bytes: PhantomData }
    }

    /// `[0, len)` of this module's own linear memory: the browser's, where
    /// the module's memory is the guest's.
    ///
    /// # Safety
    /// The module's memory from address 0 is the guest's, and nothing else
    /// in the module holds references into the part of it the guest owns.
    /// What of `[0, len)` exists is the caller's to check (the memory's live
    /// size).
    pub unsafe fn from_address_zero(len: usize) -> Span<'a> {
        Span { ptr: std::ptr::null_mut(), len, _bytes: PhantomData }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The pointer to `start`. From address zero, the address itself with
    /// the provenance the module exposed for its memory, hidden from the
    /// optimiser (LLVM otherwise reasons about what an integer-made pointer
    /// points at); otherwise into the slice.
    #[inline(always)]
    fn at(&self, start: usize) -> *mut u8 {
        if self.ptr.is_null() {
            std::hint::black_box(std::ptr::with_exposed_provenance_mut::<u8>(start))
        } else {
            self.ptr.wrapping_add(start)
        }
    }

    /// Whether `[start, start + n)` is in the span and may be sliced: not
    /// empty, and not at address 0 of the module's memory.
    #[inline(always)]
    fn ok(&self, start: usize, n: usize) -> bool {
        n != 0 && start.checked_add(n).is_some_and(|e| e <= std::hint::black_box(self.len)) && !(self.ptr.is_null() && start == 0)
    }

    /// `[start, start + n)`: empty when `n` is 0, `None` outside the span or
    /// at address 0 of the module's memory.
    #[inline(always)]
    pub fn get(&self, start: usize, n: usize) -> Option<&[u8]> {
        if n == 0 {
            return Some(&[]);
        }
        // SAFETY: in bounds and not at address 0 (`ok`); the borrow of `self`
        // keeps the memory for the slice's lifetime.
        self.ok(start, n).then(|| unsafe { std::slice::from_raw_parts(self.at(start), n) })
    }

    #[inline(always)]
    pub fn get_mut(&mut self, start: usize, n: usize) -> Option<&mut [u8]> {
        if n == 0 {
            return Some(&mut []);
        }
        // SAFETY: as `get`'s, the `&mut self` making it the one slice.
        self.ok(start, n).then(|| unsafe { std::slice::from_raw_parts_mut(self.at(start), n) })
    }

    /// `n` bytes from `src` to `dst`, overlap allowed: `false` when either
    /// range is not in the span (nothing copied).
    pub fn copy_within(&mut self, src: usize, dst: usize, n: usize) -> bool {
        if n == 0 {
            return true;
        }
        if !self.ok(src, n) || !self.ok(dst, n) {
            return false;
        }
        // SAFETY: both ranges in bounds (`ok`); `copy` allows the overlap.
        unsafe { std::ptr::copy(self.at(src), self.at(dst), n) };
        true
    }

    /// The whole span as a slice, on a host where it is one (native: a real
    /// allocation). `None` for a span from address 0.
    pub fn as_slice_mut(&mut self) -> Option<&mut [u8]> {
        // SAFETY: the span is the slice it was made from.
        (!self.ptr.is_null()).then(|| unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) })
    }
}

#[cfg(test)]
mod tests {
    use super::Span;

    #[test]
    fn ranges_of_a_slice() {
        let mut buf = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut s = Span::new(&mut buf);
        assert_eq!(s.get(0, 2), Some(&[1u8, 2][..]));
        assert_eq!(s.get(6, 2), Some(&[7u8, 8][..]));
        assert_eq!(s.get(7, 2), None);
        assert_eq!(s.get(usize::MAX, 2), None);
        assert_eq!(s.get(100, 0), Some(&[][..]));
        s.get_mut(2, 2).unwrap().copy_from_slice(&[9, 9]);
        assert!(s.copy_within(0, 1, 4));
        assert!(!s.copy_within(0, 6, 4));
        assert_eq!(buf, [1, 1, 2, 9, 9, 6, 7, 8]);
    }

    #[test]
    fn nothing_at_address_zero() {
        // (only refusals: nothing here dereferences the span)
        let mut s = unsafe { Span::from_address_zero(1 << 20) };
        assert!(s.get(0, 4).is_none());
        assert!(s.get_mut(0, 1).is_none());
        assert!(s.get((1 << 20) - 1, 2).is_none());
        assert_eq!(s.get(0, 0), Some(&[][..]));
        assert!(!s.copy_within(0, 16, 4));
        assert!(s.as_slice_mut().is_none());
    }
}
