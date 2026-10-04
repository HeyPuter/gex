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
pub mod ftab;
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
/// The x87's extended-precision arithmetic (its own crate, shared with the
/// IR interpreter and the code generators).
pub use gex_f80 as f80;
pub mod x86;
pub mod i386;
pub mod ppc;

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

    /// `len` bytes from `src` to `dst`, both guest addresses, overlap allowed:
    /// no host buffer in between (in the browser every host buffer is shim
    /// heap, and a frame-sized one per frame fragmented it to exhaustion).
    pub fn copy_within(&mut self, src: u64, dst: u64, len: usize) -> Result<(), Errno> {
        let s = self.range(src, len)?;
        let d = self.range(dst, len)?;
        self.view_mut().copy_within(s, d.start);
        Ok(())
    }

    /// As [`Self::zero`], writing only the 4 KiB pages that are not zero
    /// already. For fresh anonymous memory: reading a page the host never
    /// touched costs no memory, writing it commits it, so a large
    /// reservation zeroed with `zero` cost its whole size in resident memory
    /// (a 1 GiB `PROT_NONE` mmap took ~1 GiB of RSS natively).
    pub fn zero_dirty(&mut self, addr: u64, len: usize) -> Result<(), Errno> {
        let r = self.range(addr, len)?;
        for page in self.view_mut()[r].chunks_mut(4096) {
            // OR-folded 64 bytes at a time, which vectorises: this scan is
            // the whole cost for the common case of a clean page.
            let clean = page.chunks(64).all(|c| c.iter().fold(0u8, |a, &b| a | b) == 0);
            if !clean {
                page.fill(0);
            }
        }
        Ok(())
    }

    // The typed accessors read and write guest memory in the guest's byte
    // order, and the runtime's own structures below
    // `gex_abi::GUEST_ORDER_FROM` (thread contexts, the kernel's scratch, the
    // dispatch directory) in the host's: see `gex_abi::is_guest_be`. With a
    // little-endian guest both are little-endian and these are the plain
    // conversions.

    pub fn read_u64(&self, addr: u64) -> Result<u64, Errno> {
        let b: [u8; 8] = self.read(addr, 8)?.try_into().unwrap();
        Ok(if gex_abi::is_guest_be(addr) { u64::from_be_bytes(b) } else { u64::from_le_bytes(b) })
    }

    pub fn write_u64(&mut self, addr: u64, v: u64) -> Result<(), Errno> {
        if gex_abi::is_guest_be(addr) {
            self.write(addr, &v.to_be_bytes())
        } else {
            self.write(addr, &v.to_le_bytes())
        }
    }

    pub fn read_u32(&self, addr: u64) -> Result<u32, Errno> {
        let b: [u8; 4] = self.read(addr, 4)?.try_into().unwrap();
        Ok(if gex_abi::is_guest_be(addr) { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) })
    }

    pub fn write_u32(&mut self, addr: u64, v: u32) -> Result<(), Errno> {
        if gex_abi::is_guest_be(addr) {
            self.write(addr, &v.to_be_bytes())
        } else {
            self.write(addr, &v.to_le_bytes())
        }
    }

    /// 64 bits of a guest bitmap made of C `long`s: a `sigset_t`, an
    /// `fd_set` word. Bit `n` is signal `n + 1` or descriptor `n`. On a
    /// 64-bit or little-endian guest that is one `u64`; a 32-bit big-endian
    /// guest stores two words, the one holding bits 0-31 first.
    pub fn read_bits64(&self, addr: u64) -> Result<u64, Errno> {
        if gex_abi::BIG_ENDIAN {
            let lo = self.read_u32(addr)? as u64;
            let hi = self.read_u32(addr + 4)? as u64;
            return Ok(lo | (hi << 32));
        }
        self.read_u64(addr)
    }

    pub fn write_bits64(&mut self, addr: u64, v: u64) -> Result<(), Errno> {
        if gex_abi::BIG_ENDIAN {
            self.write_u32(addr, v as u32)?;
            return self.write_u32(addr + 4, (v >> 32) as u32);
        }
        self.write_u64(addr, v)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The byte-order rule (docs/ppc.md §3): the runtime's own structures
    /// below `GUEST_ORDER_FROM` are little-endian on every guest, guest
    /// memory above it is in the guest's order.
    #[test]
    fn typed_accessors_follow_the_guest_order_line() {
        let line = gex_abi::GUEST_ORDER_FROM;
        let mut buf = vec![0u8; line as usize + 0x100];
        let mut m = GuestMem::new(&mut buf);
        m.write_u32(0x1000, 0x1122_3344).unwrap();
        m.write_u32(line, 0x1122_3344).unwrap();
        assert_eq!(m.read(0x1000, 4).unwrap(), &[0x44, 0x33, 0x22, 0x11]);
        let above = if gex_abi::BIG_ENDIAN { [0x11, 0x22, 0x33, 0x44] } else { [0x44, 0x33, 0x22, 0x11] };
        assert_eq!(m.read(line, 4).unwrap(), &above);
        assert_eq!(m.read_u32(line).unwrap(), 0x1122_3344);
        m.write_u64(line + 8, 0x0102_0304_0506_0708).unwrap();
        assert_eq!(m.read_u64(line + 8).unwrap(), 0x0102_0304_0506_0708);
    }

    /// No guest mapping is ever placed below the line, so no guest-visible
    /// structure can be read with the runtime's byte order.
    #[test]
    fn nothing_guest_visible_lives_below_the_order_line() {
        let line = gex_abi::GUEST_ORDER_FROM;
        assert!(mem::hits_runtime(0x1000, 0x2000));
        assert!(mem::hits_runtime(line - 0x1000, line));
        assert!(!mem::hits_runtime(line, line + 0x1000));
        let mut map = mem::MemoryMap::new(gex_abi::PIE_BASE);
        let fixed = mem::flags::FIXED | mem::flags::PRIVATE | mem::flags::ANONYMOUS;
        assert!(map.mmap(0x10_0000, 0x1000, mem::prot::READ, fixed, "low").is_err());
        let any = mem::flags::PRIVATE | mem::flags::ANONYMOUS;
        let a = map.mmap(0, 0x1000, mem::prot::READ, any, "anon").unwrap();
        assert!(a >= line);
    }

    /// Every byte of the range reads as zero afterwards, whichever pages were
    /// dirty, including a range that starts and ends mid-page.
    #[test]
    fn zero_dirty_zeroes_what_was_written() {
        let mut buf = vec![0u8; 0x6000];
        buf[0x1000] = 1;
        buf[0x2fff] = 2;
        buf[0x3800] = 3;
        buf[0x5fff] = 4;
        let mut m = GuestMem::new(&mut buf);
        m.zero_dirty(0x1000, 0x4000).unwrap();
        assert!(m.read(0x1000, 0x4000).unwrap().iter().all(|&b| b == 0));
        // Outside the range is left alone.
        assert_eq!(m.read(0x5fff, 1).unwrap()[0], 4);
        let mut buf = vec![7u8; 0x3000];
        let mut m = GuestMem::new(&mut buf);
        m.zero_dirty(0x800, 0x1000).unwrap();
        let b = m.read(0, 0x3000).unwrap();
        assert!(b[0x800..0x1800].iter().all(|&x| x == 0));
        assert!(b[..0x800].iter().chain(&b[0x1800..]).all(|&x| x == 7));
    }
}

/// Bumped (after the push, under the kernel lock) whenever code is mapped or
/// retired (`Kernel::new_code`, `Kernel::retired_code`): a host checks this
/// before taking the kernel lock to drain them, on every syscall.
pub static CODE_EVENTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn note_code_change() {
    CODE_EVENTS.fetch_add(1, std::sync::atomic::Ordering::Release);
}
