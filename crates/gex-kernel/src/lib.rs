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
pub mod fs;
pub mod mem;
pub mod syscalls;

pub use errno::Errno;
pub use syscalls::{Kernel, SyscallOutcome};

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

    fn range(&self, addr: u64, len: usize) -> Result<std::ops::Range<usize>, Errno> {
        let start = addr as usize;
        let end = start.checked_add(len).ok_or(Errno::EFAULT)?;
        if addr > usize::MAX as u64 || end > self.bytes.len() {
            return Err(Errno::EFAULT);
        }
        Ok(start..end)
    }

    pub fn read(&self, addr: u64, len: usize) -> Result<&[u8], Errno> {
        Ok(&self.bytes[self.range(addr, len)?])
    }

    pub fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), Errno> {
        let r = self.range(addr, data.len())?;
        self.bytes[r].copy_from_slice(data);
        Ok(())
    }

    pub fn zero(&mut self, addr: u64, len: usize) -> Result<(), Errno> {
        let r = self.range(addr, len)?;
        self.bytes[r].fill(0);
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
        if start >= self.bytes.len() {
            return Err(Errno::EFAULT);
        }
        let end = (start + max).min(self.bytes.len());
        let slice = &self.bytes[start..end];
        match slice.iter().position(|b| *b == 0) {
            Some(n) => Ok(slice[..n].to_vec()),
            None => Err(Errno::ENAMETOOLONG),
        }
    }
}
