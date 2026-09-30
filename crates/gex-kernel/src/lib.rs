//! `gex-kernel` — the Linux personality.
//!
//! A syscall implementation, not a machine emulator (PLAN §2.2). The kernel is
//! a plain Rust library that operates on the guest's linear memory as a byte
//! slice, which is what lets the same code be linked into a native host today
//! and compiled to wasm for the browser later without a second implementation.
//!
//! M0 covers tier T0 from PLAN §8 — enough for a static musl program to
//! start, do I/O and exit — plus the file-descriptor and VFS plumbing the
//! later tiers build on.

#![forbid(unsafe_code)]

pub mod errno;
pub mod eventfd;
pub mod file;
pub mod fs;
pub mod futex;
pub mod mem;
pub mod net;
pub mod nt;
pub mod pipe;
pub mod proc;
pub mod pty;
pub mod signal;
pub mod stdio;
pub mod syscalls;
pub mod timerfd;
pub mod tty;
pub mod vfs;
pub mod unix;
pub mod x86;

pub use errno::Errno;
pub use proc::{ProcTable, INIT_PID};
pub use syscalls::{
    skips_proc_snapshot, system_clock, BlockOp, Child, ChildHandle, ClockFn, CodeMap, Kernel,
    KernelProcSource, ProcState, SyscallOutcome, ThreadState,
};

/// Guest linear memory, addressed exactly as the guest addresses it.
///
/// Guest virtual address equals byte offset (PLAN §2.3), so the kernel needs
/// no translation layer — just bounds checks, which are also the syscall
/// ABI's `EFAULT` checks.
pub struct GuestMem<'a> {
    pub bytes: &'a mut [u8],
}

impl<'a> GuestMem<'a> {
    pub fn new(bytes: &'a mut [u8]) -> GuestMem<'a> {
        GuestMem { bytes }
    }

    /// The backing bytes, hidden from the optimiser on wasm.
    ///
    /// In the browser this slice spans the guest's whole address space via a
    /// pointer made from an integer. LLVM sees no provenance, assumes size zero,
    /// and folds the bounds checks below to "always out of range" at some call
    /// sites but not others. A barrier at each use leaves it nothing to fold;
    /// native hosts get a real slice and need none of this.
    #[inline(always)]
    fn view(&self) -> &[u8] {
        #[cfg(target_family = "wasm")]
        {
            std::hint::black_box(&*self.bytes)
        }
        #[cfg(not(target_family = "wasm"))]
        {
            self.bytes
        }
    }

    #[inline(always)]
    fn view_mut(&mut self) -> &mut [u8] {
        #[cfg(target_family = "wasm")]
        {
            std::hint::black_box(&mut *self.bytes)
        }
        #[cfg(not(target_family = "wasm"))]
        {
            self.bytes
        }
    }

    fn range(&self, addr: u64, len: usize) -> Result<std::ops::Range<usize>, Errno> {
        let start = addr as usize;
        let end = start.checked_add(len).ok_or(Errno::EFAULT)?;
        if addr > usize::MAX as u64 || end > self.limit() {
            return Err(Errno::EFAULT);
        }
        Ok(start..end)
    }

    /// The last address this memory really has, read live.
    ///
    /// Not `view().len()`: in the browser the view spans the whole 2 GiB a
    /// guest pointer may name, while the memory starts smaller and grows as the
    /// guest maps, so a check against the view passed for unbacked addresses and
    /// the access trapped. Live because it moves during a syscall -- an `mmap`
    /// grows the memory then zeroes what it just mapped.
    #[inline(always)]
    fn limit(&self) -> usize {
        #[cfg(target_family = "wasm")]
        {
            let live = (core::arch::wasm32::memory_size(0) as u64) << 16;
            (live as usize).min(self.view().len())
        }
        #[cfg(not(target_family = "wasm"))]
        {
            self.view().len()
        }
    }

    pub fn read(&self, addr: u64, len: usize) -> Result<&[u8], Errno> {
        let r = self.range(addr, len)?;
        Ok(&self.view()[r])
    }

    /// A writable slice of guest memory, for a caller that fills it itself.
    ///
    /// Without it a `read` allocates a guest-sized buffer, fills it, and copies
    /// in -- and the guest chooses the size; that made one `apt-get update`
    /// claim 117 arena spans (1,872 MiB). See task 140.
    pub fn read_mut(&mut self, addr: u64, len: usize) -> Result<&mut [u8], Errno> {
        let r = self.range(addr, len)?;
        Ok(&mut self.view_mut()[r])
    }

    pub fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), Errno> {
        let r = self.range(addr, data.len())?;
        self.view_mut()[r].copy_from_slice(data);
        Ok(())
    }

    pub fn zero(&mut self, addr: u64, len: usize) -> Result<(), Errno> {
        let r = self.range(addr, len)?;
        self.view_mut()[r].fill(0);
        Ok(())
    }

    pub fn read_u64(&self, addr: u64) -> Result<u64, Errno> {
        let b = self.read(addr, 8)?;
        Ok(u64::from_le_bytes(b.try_into().unwrap()))
    }

    pub fn write_u64(&mut self, addr: u64, v: u64) -> Result<(), Errno> {
        self.write(addr, &v.to_le_bytes())
    }

    pub fn read_u32(&self, addr: u64) -> Result<u32, Errno> {
        let b = self.read(addr, 4)?;
        Ok(u32::from_le_bytes(b.try_into().unwrap()))
    }

    pub fn write_u32(&mut self, addr: u64, v: u32) -> Result<(), Errno> {
        self.write(addr, &v.to_le_bytes())
    }

    /// Read a NUL-terminated guest string, bounded by `max` bytes.
    pub fn read_cstr(&self, addr: u64, max: usize) -> Result<Vec<u8>, Errno> {
        let start = addr as usize;
        let limit = self.limit();
        if start >= limit {
            return Err(Errno::EFAULT);
        }
        // Clamped to what exists, not what the view claims: a path near the top
        // of the stack is within a page of memory's end, and scanning `max`
        // bytes ran off into pages never mapped.
        let end = (start + max).min(limit);
        let slice = &self.view()[start..end];
        match slice.iter().position(|b| *b == 0) {
            Some(n) => Ok(slice[..n].to_vec()),
            None => Err(Errno::ENAMETOOLONG),
        }
    }
}
