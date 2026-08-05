//! Access to the guest's linear memory.
//!
//! The guest's memory is a wasm *shared* memory, because every guest thread
//! runs in its own wasmtime store and a store-owned memory would give each of
//! them a private address space. wasmtime will not hand out a `&mut [u8]` for
//! one: its only accessor is `SharedMemory::data`, which returns
//! `&[UnsafeCell<u8>]`, and that is correct — another thread may be writing to
//! any of those bytes right now.
//!
//! # Why this crate exists
//!
//! Guest memory is shared mutable state by construction. No Rust type can
//! describe it faithfully, and wrapping every guest load and store in an
//! atomic would be both wrong (the guest's own atomics are what decide the
//! ordering) and ruinous. PLAN §5.8 states the contract directly: a data race
//! in the guest stays a data race, observed as whatever the hardware does,
//! exactly as it would be on real AArch64.
//!
//! So the host reads and writes these bytes as an ordinary slice, and this
//! crate is where that decision is written down rather than scattered. The
//! host itself keeps `#![forbid(unsafe_code)]`.
//!
//! What the host is *not* allowed to assume, and does not:
//!
//! * that a value read twice is the same value — every kernel path reads a
//!   guest word once and works from the copy;
//! * that a slice handed to it stays valid across a guest instruction — the
//!   borrow lives only for the duration of one host call;
//! * that a string or buffer the guest supplied is unchanged while it is read.
//!
//! A guest that violates its own memory model gets nonsense, which is what it
//! would get on hardware.

use wasmtime::SharedMemory;

/// The guest's linear memory, viewable as bytes.
#[derive(Clone)]
pub struct GuestMemory {
    inner: SharedMemory,
}

impl GuestMemory {
    pub fn new(inner: SharedMemory) -> GuestMemory {
        GuestMemory { inner }
    }

    /// Total size in bytes. Fixed for the life of the process: gex sizes the
    /// guest's address space up front and never grows it.
    pub fn len(&self) -> usize {
        self.inner.data_size()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The whole address space as a mutable slice.
    ///
    /// See the module comment for why this is not `unsafe` despite what it
    /// does. The lifetime is tied to the borrow of `self`, which in practice
    /// means one host call.
    #[allow(clippy::mut_from_ref)]
    pub fn bytes(&self) -> &mut [u8] {
        let cells = self.inner.data();
        // SAFETY: `cells` is the guest's address space, which the guest may be
        // writing to from another thread. That is the point: see above. The
        // pointer is valid and the length correct for as long as the
        // `SharedMemory` lives, and this borrow cannot outlive `self`.
        unsafe { std::slice::from_raw_parts_mut(cells.as_ptr() as *mut u8, cells.len()) }
    }

    pub fn shared(&self) -> &SharedMemory {
        &self.inner
    }
}
