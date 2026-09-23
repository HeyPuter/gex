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
pub mod i386;
pub mod mem;
pub mod net;
pub mod pipe;
pub mod proc;
pub mod pty;
pub mod signal;
pub mod stdio;
pub mod syscalls;
pub mod timerfd;
pub mod tty;
pub mod unix;
pub mod vfs;
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
/// The highest guest address the runtime has actually backed, in bytes.
///
/// The shim updates this after every `memory.grow` (its own `memory_size` read is
/// authoritative and fresh). `GuestMem::limit` also consults it because a
/// `memory.size` instruction read from *this* crate can lag a grow another
/// Worker just made on the shared memory -- a stale-low bound that made a
/// perfectly backed write (a fork/exec child's `mmap(0, 0x2000)` above the 2 GiB
/// KUSER_SHARED_DATA line) fault EFAULT and take down wineserver. Taking the max
/// of the live read and this floor closes that window; it only ever rises.
pub static GUEST_MEM_TOP: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

/// Shim-provided helpers to reach guest memory above the 2 GiB `GuestMem` view
/// slice (`isize::MAX` on wasm32). A WoW64 guest maps and touches memory up to
/// 4 GiB, but a safe slice cannot span it and this crate forbids `unsafe`, so
/// the shim (which may use raw pointers) does the write/zero there. `None` off
/// wasm and until the shim registers them; a would-be high access then EFAULTs.
pub struct HiMem {
    pub write: fn(u64, &[u8]),
    pub zero: fn(u64, usize),
    /// Copy `buf.len()` bytes from guest memory at `addr` into `buf`. The
    /// read counterpart of `write`: a borrow into the >2 GiB half cannot be a
    /// wasm32 slice, so a caller that needs those bytes copies them out.
    pub read: fn(u64, &mut [u8]),
}

pub static HI_MEM: std::sync::OnceLock<HiMem> = std::sync::OnceLock::new();

/// Register the shim's high-memory helpers. Idempotent; the first wins.
pub fn set_hi_mem(h: HiMem) {
    let _ = HI_MEM.set(h);
}

/// A console the kernel can reach (its own `eprintln!` goes nowhere on wasm).
/// The shim installs one that routes to the browser console; native builds
/// leave it unset and the kernel stays silent.
pub static KLOG: std::sync::OnceLock<fn(&str)> = std::sync::OnceLock::new();

/// Install the kernel's console sink. Idempotent; the first wins.
pub fn set_klog(f: fn(&str)) {
    let _ = KLOG.set(f);
}

/// Print one diagnostic line through the shim's console, if one is installed.
pub fn klog(s: &str) {
    if let Some(f) = KLOG.get() {
        f(s);
    }
}

/// A cross-Worker backing for a `MemFd`, installed by the browser shim. A
/// `MemFd`'s own `Vec` is a Worker's private memory, so a memfd passed over a
/// cross-Worker socket used to be a frozen content snapshot -- which broke
/// Wine, whose client and wineserver share a single-writer memfd and expect to
/// see each other's writes. With this installed, a shared memfd keeps its bytes
/// in the global kernel (one SAB every Worker sees) and every `MemFd` that
/// names the same global id reads and writes the same bytes. Native builds leave
/// it unset and `MemFd` keeps its private `Vec` (there, processes share the one
/// `Arc<MemFd>` and no snapshot is taken).
#[derive(Clone, Copy)]
pub struct MemFdBacking {
    /// Make a global memfd, returning its id (>0).
    pub create: fn(&str) -> i32,
    /// Read `buf.len()` bytes from `offset`; returns bytes read.
    pub read: fn(i32, u64, &mut [u8]) -> usize,
    /// Write `buf` at `offset`, growing as needed; returns bytes written.
    pub write: fn(i32, u64, &[u8]) -> usize,
    /// Current length.
    pub len: fn(i32) -> u64,
    /// Grow/shrink to `len`.
    pub truncate: fn(i32, u64),
    /// Current seal bits.
    pub seals_get: fn(i32) -> u32,
    /// Add seal bits.
    pub seals_add: fn(i32, u32),
}

pub static MEMFD_BACKING: std::sync::OnceLock<MemFdBacking> = std::sync::OnceLock::new();

/// Install the cross-Worker memfd backing. Idempotent; the first wins.
pub fn set_memfd_backing(b: MemFdBacking) {
    let _ = MEMFD_BACKING.set(b);
}

/// The installed backing, if any (browser only).
pub fn memfd_backing() -> Option<&'static MemFdBacking> {
    MEMFD_BACKING.get()
}

/// A shared monotonic sequence (in the global kernel, so it orders events
/// across Workers reliably where console-log arrival order does not).
pub static KSEQ: std::sync::OnceLock<fn() -> u64> = std::sync::OnceLock::new();
pub fn set_kseq(f: fn() -> u64) {
    let _ = KSEQ.set(f);
}
pub fn kseq() -> u64 {
    KSEQ.get().map(|f| f()).unwrap_or(0)
}

/// TEMP: read any global socket's transmit-buffer length by id, from any
/// Worker -- so one Worker can check whether another's writes are visible.
pub static SOCK_TXLEN: std::sync::OnceLock<fn(i32) -> i64> = std::sync::OnceLock::new();
pub fn set_sock_txlen(f: fn(i32) -> i64) {
    let _ = SOCK_TXLEN.set(f);
}
pub fn sock_txlen(id: i32) -> i64 {
    SOCK_TXLEN.get().map(|f| f(id)).unwrap_or(-2)
}


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
            // `memory.size` read from this crate can lag a grow another Worker
            // just made on the shared memory, so take the max with the top the
            // shim recorded after its own (fresh) read. See `GUEST_MEM_TOP`.
            let live = (core::arch::wasm32::memory_size(0) as u64) << 16;
            let floor = GUEST_MEM_TOP.load(core::sync::atomic::Ordering::Relaxed);
            // Not capped by `view().len()`: that slice tops out at 2 GiB
            // (`isize::MAX` on wasm32), but a WoW64 guest maps and the kernel
            // must touch memory up to 4 GiB. `read`/`write`/`zero` below reach
            // it by addressing the byte directly rather than indexing the view,
            // so the only bound is the memory that is really backed.
            live.max(floor) as usize
        }
        #[cfg(not(target_family = "wasm"))]
        {
            self.view().len()
        }
    }


    /// Whether an access ending at `end` is above the 2 GiB view slice on wasm
    /// (`isize::MAX`), so it must be reached through the shim's raw-pointer
    /// helpers rather than by indexing the view. Always false off wasm.
    #[inline(always)]
    fn above_view(&self, end: usize) -> bool {
        #[cfg(target_family = "wasm")]
        {
            end > self.view().len()
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = end;
            false
        }
    }

    pub fn read(&self, addr: u64, len: usize) -> Result<&[u8], Errno> {
        let r = self.range(addr, len)?;
        // A borrow into the >2 GiB half cannot be a slice on wasm32; a caller
        // needing those bytes must copy them out (`read_into`).
        if self.above_view(r.end) {
            return Err(Errno::EFAULT);
        }
        Ok(&self.view()[r])
    }

    /// A writable slice of guest memory, for a caller that fills it itself.
    ///
    /// Without it a `read` allocates a guest-sized buffer, fills it, and copies
    /// in -- and the guest chooses the size; that made one `apt-get update`
    /// claim 117 arena spans (1,872 MiB). See task 140.
    /// Whether [addr, addr+len) lies above the 2 GiB view slice, so it cannot
    /// be handed out as a `&mut [u8]` and a caller filling guest memory must go
    /// through a temp buffer + `write` (which reaches the high half via HI_MEM).
    pub fn is_high(&self, addr: u64, len: usize) -> bool {
        match self.range(addr, len) {
            Ok(r) => self.above_view(r.end),
            Err(_) => false,
        }
    }

    pub fn read_mut(&mut self, addr: u64, len: usize) -> Result<&mut [u8], Errno> {
        let r = self.range(addr, len)?;
        if self.above_view(r.end) {
            return Err(Errno::EFAULT);
        }
        Ok(&mut self.view_mut()[r])
    }

    pub fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), Errno> {
        let r = self.range(addr, data.len())?;
        // The >2 GiB half is not in the view slice on wasm32; the shim reaches
        // it with raw pointers (a WoW64 guest maps there and the kernel must
        // write it -- e.g. an anonymous mmap's zero-fill of a 2 GiB+ page).
        if self.above_view(r.end) {
            if let Some(h) = HI_MEM.get() {
                (h.write)(addr, data);
                return Ok(());
            }
            return Err(Errno::EFAULT);
        }
        self.view_mut()[r].copy_from_slice(data);
        Ok(())
    }

    pub fn zero(&mut self, addr: u64, len: usize) -> Result<(), Errno> {
        let r = self.range(addr, len)?;
        if self.above_view(r.end) {
            if let Some(h) = HI_MEM.get() {
                (h.zero)(addr, len);
                return Ok(());
            }
            return Err(Errno::EFAULT);
        }
        self.view_mut()[r].fill(0);
        Ok(())
    }

    /// Copy `buf.len()` guest bytes at `addr` into `buf`, reaching above the
    /// 2 GiB view through the shim's raw-pointer helper. Unlike `read` this
    /// works for the >2 GiB half (a WoW64 guest's stack lives up there, so path
    /// and struct pointers the loader passes are above 2 GiB -- native, with a
    /// flat address space, never sees this). Without it every such read EFAULTs
    /// and a builtin DLL load fails with STATUS_ACCESS_VIOLATION.
    pub fn read_into(&self, addr: u64, buf: &mut [u8]) -> Result<(), Errno> {
        let r = self.range(addr, buf.len())?;
        if self.above_view(r.end) {
            if let Some(h) = HI_MEM.get() {
                (h.read)(addr, buf);
                return Ok(());
            }
            return Err(Errno::EFAULT);
        }
        buf.copy_from_slice(&self.view()[r]);
        Ok(())
    }

    /// Owned copy of `len` guest bytes at `addr`, reaching above the 2 GiB view
    /// via HI_MEM. Use when the bytes are copied out anyway (a write/send from
    /// a WoW64 guest's >2 GiB stack buffer) -- `read` returns a borrow and so
    /// EFAULTs there, which ntdll turns into c0000005.
    pub fn read_owned(&self, addr: u64, len: usize) -> Result<Vec<u8>, Errno> {
        let mut buf = vec![0u8; len];
        self.read_into(addr, &mut buf)?;
        Ok(buf)
    }

    pub fn read_u64(&self, addr: u64) -> Result<u64, Errno> {
        let mut b = [0u8; 8];
        self.read_into(addr, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    pub fn write_u64(&mut self, addr: u64, v: u64) -> Result<(), Errno> {
        self.write(addr, &v.to_le_bytes())
    }

    pub fn read_u32(&self, addr: u64) -> Result<u32, Errno> {
        let mut b = [0u8; 4];
        self.read_into(addr, &mut b)?;
        Ok(u32::from_le_bytes(b))
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
        // Above the 2 GiB view slice (a WoW64 guest's stack lives there, so its
        // path pointers do too), the view cannot be indexed -- copy the bytes
        // out through the shim helper and scan the copy. Native never gets here.
        if self.above_view(end) {
            let mut buf = vec![0u8; end - start];
            self.read_into(addr, &mut buf)?;
            return match buf.iter().position(|b| *b == 0) {
                Some(n) => {
                    buf.truncate(n);
                    Ok(buf)
                }
                None => Err(Errno::ENAMETOOLONG),
            };
        }
        let slice = &self.view()[start..end];
        match slice.iter().position(|b| *b == 0) {
            Some(n) => Ok(slice[..n].to_vec()),
            None => Err(Errno::ENAMETOOLONG),
        }
    }
}
