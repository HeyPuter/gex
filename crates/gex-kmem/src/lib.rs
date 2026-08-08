//! Reaching the calling process's memory from the global kernel module.
//!
//! The kernel is one module with one memory, shared by every Worker of every
//! process, so that a syscall is a direct call and the file table, the process
//! table and the pipes are simply *there*. A guest's buffers are not: they
//! live in that process's own memory, which the kernel module imports as a
//! second memory and cannot address with an ordinary pointer.
//!
//! `memory.copy` can name two memories, and that is the whole interface. Rust
//! has no syntax for it -- a `*mut u8` is address space 0 and `llvm.memcpy`
//! between spaces emits `dst=0 src=0` -- so the two instructions here are
//! written as inline assembly, which passes them through unchanged.
//!
//! What is *not* possible, and what shapes everything above: a scalar load or
//! store against memory 1. The assembler has no way to put a memory index in
//! a memarg (`i32.load 1` reads the 1 as an offset). So a syscall that wants
//! a word from the guest copies eight bytes into its own memory and reads
//! them there. That is a real cost on small reads and no cost at all on the
//! ones that matter, which are buffers.

#![no_std]
#![feature(asm_experimental_arch)]

/// Which memory the process's address space is imported as.
///
/// The kernel module's own memory is 0 -- that is what `--import-memory`
/// gives it and what all its Rust addresses mean. The process memory is
/// appended to the import section afterwards, and imports are the bottom of
/// the index space, so it lands at 1.
pub const PROCESS_MEMORY: u32 = 1;

/// Copy `len` bytes from the calling process's memory into this one.
///
/// # Safety
/// `dst` must be valid for `len` bytes in the kernel's own memory, and
/// `src..src+len` must be in bounds of the process memory -- the engine traps
/// rather than corrupting anything if it is not, but a trap here unwinds a
/// syscall that has already taken the kernel lock.
#[inline]
pub unsafe fn pull(dst: *mut u8, src: u32, len: u32) {
    unsafe {
        core::arch::asm!(
            "local.get {dst}",
            "local.get {src}",
            "local.get {len}",
            "memory.copy 0, 1",
            dst = in(local) dst,
            src = in(local) src,
            len = in(local) len,
        );
    }
}

/// Copy `len` bytes from this memory into the calling process's.
///
/// # Safety
/// As [`pull`], with the roles reversed.
#[inline]
pub unsafe fn push(dst: u32, src: *const u8, len: u32) {
    unsafe {
        core::arch::asm!(
            "local.get {dst}",
            "local.get {src}",
            "local.get {len}",
            "memory.copy 1, 0",
            dst = in(local) dst,
            src = in(local) src,
            len = in(local) len,
        );
    }
}

/// Read one `u64` out of the guest, the long way round.
///
/// Every scalar read of guest memory is a copy, because there is no load that
/// can name a second memory. Eight bytes through `memory.copy` is a few
/// instructions more than a load and is not on any path that matters.
///
/// # Safety
/// `at..at+8` must be in bounds of the process memory.
#[inline]
pub unsafe fn read_u64(at: u32) -> u64 {
    let mut buf = [0u8; 8];
    unsafe { pull(buf.as_mut_ptr(), at, 8) };
    u64::from_le_bytes(buf)
}

/// Write one `u64` into the guest. See [`read_u64`].
///
/// # Safety
/// `at..at+8` must be in bounds of the process memory.
#[inline]
pub unsafe fn write_u64(at: u32, v: u64) {
    let buf = v.to_le_bytes();
    unsafe { push(at, buf.as_ptr(), 8) };
}
