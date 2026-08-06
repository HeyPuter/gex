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
pub mod file;
pub mod fs;
pub mod futex;
pub mod mem;
pub mod pipe;
pub mod proc;
pub mod signal;
pub mod stdio;
pub mod syscalls;
pub mod vfs;

pub use errno::Errno;
pub use proc::{ProcTable, INIT_PID};
pub use syscalls::{
    skips_proc_snapshot, system_clock, BlockOp, Child, ChildHandle, ClockFn, Kernel,
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
    /// In the browser host this slice covers the guest's whole address space,
    /// which is the host's *own* linear memory reached through a pointer made
    /// from an integer. LLVM sees a pointer with no provenance, decides the
    /// allocation behind it has size zero, and folds the bounds checks below
    /// to "always out of range" -- but only at some call sites, so the same
    /// read succeeded from one function and returned `EFAULT` from another a
    /// microsecond later. A barrier at each use leaves it nothing to fold.
    ///
    /// Native hosts get a real slice from wasmtime and need none of this.
    #[inline(always)]
    fn view(&self) -> &[u8] {
        #[cfg(target_arch = "wasm32")]
        {
            std::hint::black_box(&*self.bytes)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.bytes
        }
    }

    #[inline(always)]
    fn view_mut(&mut self) -> &mut [u8] {
        #[cfg(target_arch = "wasm32")]
        {
            std::hint::black_box(&mut *self.bytes)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.bytes
        }
    }

    fn range(&self, addr: u64, len: usize) -> Result<std::ops::Range<usize>, Errno> {
        let start = addr as usize;
        let end = start.checked_add(len).ok_or(Errno::EFAULT)?;
        if addr > usize::MAX as u64 || end > self.view().len() {
            return Err(Errno::EFAULT);
        }
        Ok(start..end)
    }

    pub fn read(&self, addr: u64, len: usize) -> Result<&[u8], Errno> {
        let r = self.range(addr, len)?;
        Ok(&self.view()[r])
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
        if start >= self.view().len() {
            return Err(Errno::EFAULT);
        }
        let end = (start + max).min(self.view().len());
        let slice = &self.view()[start..end];
        match slice.iter().position(|b| *b == 0) {
            Some(n) => Ok(slice[..n].to_vec()),
            None => Err(Errno::ENAMETOOLONG),
        }
    }
}
