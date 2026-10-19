//! Reaching the calling process's memory from the global kernel module.
//!
//! The kernel is one module with one memory, shared by every Worker of every
//! process. A guest's buffers are not in it: they live in that process's own
//! memory, imported as a second memory that an ordinary pointer cannot address.
//!
//! `memory.copy` can name two memories and is the whole interface. Rust has no
//! syntax for it (a `*mut u8` is always address space 0), so the copies here are
//! inline assembly. A scalar load or store against memory 1 is *not* possible --
//! the assembler cannot put a memory index in a memarg -- so a syscall reading a
//! word copies eight bytes into its own memory first. Real cost on small reads,
//! none on the buffers that matter.

#![no_std]
#![feature(asm_experimental_arch)]

/// Which memories the process's address space is imported as. The kernel's
/// own is 0; a process's is appended to the import section after it, as two
/// imports (`tools/add-memory.mjs`): 1, a 32-bit memory, and 2, a memory64
/// (docs/mem64.md step 6). A Worker gives its kernel instance its process's
/// memory as the one of its type and an empty one (no pages) as the other:
/// one kernel module serves both flavours, so a page whose processes differ
/// (W64's wasm32 server beside a 64-bit memory's loader) has one kernel data
/// layout. Two builds, one per flavour, put their statics 16 bytes apart and
/// read each other's state wrong in one kernel memory.
pub const PROCESS_MEMORY: u32 = 1;
/// The memory64 one. See [`PROCESS_MEMORY`].
pub const PROCESS_MEMORY64: u32 = 2;

/// An address in the process memory: 64-bit, for either flavour (a 32-bit
/// memory's are below 4 GiB), as `gex_kernel::PAddr`.
pub type PAddr = u64;

/// Pages of the memory64 process memory: 0 in a 32-bit memory's Worker,
/// whose kernel instance has an empty one there.
#[inline]
fn pages64() -> u64 {
    let n: u64;
    unsafe {
        core::arch::asm!(
            "memory.size 2",
            "local.set {n}",
            n = out(local) n,
        );
    }
    n
}

/// Pages of the 32-bit process memory.
#[inline]
fn pages32() -> u32 {
    let n: u32;
    unsafe {
        core::arch::asm!(
            "memory.size 1",
            "local.set {n}",
            n = out(local) n,
        );
    }
    n
}

/// How many 64 KiB pages the calling process's memory has.
///
/// `memory.size` can name a memory index (unlike a load), so this is how the
/// kernel tells a valid guest pointer from one off the end and answers `EFAULT`
/// rather than trapping the Worker mid-syscall with the kernel lock held.
#[inline]
pub fn process_pages() -> PAddr {
    match pages64() {
        0 => pages32() as u64,
        n => n,
    }
}

/// Whether `at..at+len` is inside the calling process's memory. Every `push`
/// and `pull` should be guarded by this: `memory.copy` traps rather than
/// returning, and a trap mid-syscall kills the Worker holding the kernel lock.
#[inline]
pub fn in_process(at: PAddr, len: u32) -> bool {
    let end = match at.checked_add(len as PAddr) {
        Some(e) => e,
        None => return false,
    };
    end <= process_pages() << 16
}

/// Copy `len` bytes from the calling process's memory into this one.
///
/// # Safety
/// `dst` must be valid for `len` bytes in the kernel's own memory, and
/// `src..src+len` must be in bounds of the process memory -- the engine traps
/// rather than corrupting anything if it is not, but a trap here unwinds a
/// syscall that has already taken the kernel lock.
#[inline]
pub unsafe fn pull(dst: *mut u8, src: PAddr, len: u32) {
    // A memory64 takes an i64 address; the kernel's own (and the length,
    // the smaller of the two index types) stay i32.
    if pages64() != 0 {
        unsafe {
            core::arch::asm!(
                "local.get {dst}",
                "local.get {src}",
                "local.get {len}",
                "memory.copy 0, 2",
                dst = in(local) dst,
                src = in(local) src,
                len = in(local) len,
            );
        }
    } else {
        let src = src as u32;
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
}

/// Copy `len` bytes from this memory into the calling process's.
///
/// # Safety
/// As [`pull`], with the roles reversed.
#[inline]
pub unsafe fn push(dst: PAddr, src: *const u8, len: u32) {
    if pages64() != 0 {
        unsafe {
            core::arch::asm!(
                "local.get {dst}",
                "local.get {src}",
                "local.get {len}",
                "memory.copy 2, 0",
                dst = in(local) dst,
                src = in(local) src,
                len = in(local) len,
            );
        }
    } else {
        let dst = dst as u32;
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
}

/// Zero `len` bytes of the calling process's memory at `dst`.
///
/// # Safety
/// `dst..dst+len` must be in bounds of the process memory.
#[inline]
pub unsafe fn zero(dst: PAddr, len: u32) {
    if pages64() != 0 {
        // A memory64's `memory.fill` takes its length as an i64, like its address.
        let len = len as u64;
        unsafe {
            core::arch::asm!(
                "local.get {dst}",
                "i32.const 0",
                "local.get {len}",
                "memory.fill 2",
                dst = in(local) dst,
                len = in(local) len,
            );
        }
    } else {
        let dst = dst as u32;
        unsafe {
            core::arch::asm!(
                "local.get {dst}",
                "i32.const 0",
                "local.get {len}",
                "memory.fill 1",
                dst = in(local) dst,
                len = in(local) len,
            );
        }
    }
}

/// Read one `u64` out of the guest, via a copy: no load can name a second
/// memory.
///
/// # Safety
/// `at..at+8` must be in bounds of the process memory.
#[inline]
pub unsafe fn read_u64(at: PAddr) -> u64 {
    let mut buf = [0u8; 8];
    unsafe { pull(buf.as_mut_ptr(), at, 8) };
    u64::from_le_bytes(buf)
}

/// Write one `u64` into the guest. See [`read_u64`].
///
/// # Safety
/// `at..at+8` must be in bounds of the process memory.
#[inline]
pub unsafe fn write_u64(at: PAddr, v: u64) {
    let buf = v.to_le_bytes();
    unsafe { push(at, buf.as_ptr(), 8) };
}

/// [`pull`], but answering `false` instead of trapping on a bad range. The
/// kernel is called with guest-chosen addresses, and an out-of-range
/// `memory.copy` would trap the Worker inside a syscall with the kernel lock
/// held.
///
/// # Safety
/// `dst` must be valid for `len` bytes in the kernel's own memory. The process
/// side is checked here.
#[inline]
pub unsafe fn pull_checked(dst: *mut u8, src: PAddr, len: u32) -> bool {
    if !in_process(src, len) {
        return false;
    }
    unsafe { pull(dst, src, len) };
    true
}

/// [`push`], but answering `false` instead of trapping. See [`pull_checked`].
///
/// # Safety
/// `src` must be valid for `len` bytes in the kernel's own memory.
#[inline]
pub unsafe fn push_checked(dst: PAddr, src: *const u8, len: u32) -> bool {
    if !in_process(dst, len) {
        return false;
    }
    unsafe { push(dst, src, len) };
    true
}
