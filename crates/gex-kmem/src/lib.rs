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

/// How many 64 KiB pages the calling process's memory has.
///
/// `memory.size` can name a memory index, so unlike a load this one is
/// expressible -- and it is the only way the kernel can tell a pointer a
/// process handed it from a pointer that is off the end. Without it every
/// bad address a guest passes to a syscall is a trap that kills the Worker
/// mid-call, with the kernel lock still held and nothing said about which
/// call it was. With it, the answer is `EFAULT`, which is what Linux says.
#[inline]
pub fn process_pages() -> u32 {
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

/// Whether `at..at+len` is inside the calling process's memory.
///
/// Every `push` and `pull` should be guarded by this. The copy itself cannot
/// check -- `memory.copy` traps rather than returning -- and a trap inside a
/// syscall is the worst shape of failure available: the Worker dies holding
/// the kernel lock, and the only evidence is `memory access out of bounds`.
#[inline]
pub fn in_process(at: u32, len: u32) -> bool {
    let end = match at.checked_add(len) {
        Some(e) => e as u64,
        None => return false,
    };
    end <= (process_pages() as u64) << 16
}

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

/// [`pull`], but answering `false` instead of trapping on a bad range.
///
/// The kernel is called with addresses a guest chose. `memory.copy` traps on
/// an out-of-range one, and a trap here is the worst failure this system has:
/// the Worker dies inside a syscall, still holding the kernel lock, and the
/// only evidence is `memory access out of bounds` with no idea which call it
/// was. apt's `file` method died exactly that way, inside `newfstatat`.
///
/// # Safety
/// `dst` must be valid for `len` bytes in the kernel's own memory. The
/// process side is checked here.
#[inline]
pub unsafe fn pull_checked(dst: *mut u8, src: u32, len: u32) -> bool {
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
pub unsafe fn push_checked(dst: u32, src: *const u8, len: u32) -> bool {
    if !in_process(dst, len) {
        return false;
    }
    unsafe { push(dst, src, len) };
    true
}
