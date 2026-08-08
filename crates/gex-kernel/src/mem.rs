//! The guest's virtual memory bookkeeping.
//!
//! gex *is* the allocator for the guest address space (PLAN §2.3), so `mmap`,
//! `brk` and `munmap` are ordinary data-structure operations over a list of
//! mappings — there is no host `mmap` involved and no page table to program.
//!
//! Protection bits are recorded, not enforced: wasm has no way to fault on a
//! stray load or store (PLAN §7.2). They still matter, because `/proc/self/maps`
//! reports them, translation invalidation keys off `PROT_EXEC`, and any access
//! the *kernel* performs on the guest's behalf does check them.

use crate::errno::{Errno, SysResult};
use gex_abi::{GUEST_LIMIT, MMAP_BOTTOM, MMAP_TOP};

pub const PAGE_SIZE: u64 = 4096;

/// `mmap` protection bits.
pub mod prot {
    pub const NONE: u32 = 0;
    pub const READ: u32 = 1;
    pub const WRITE: u32 = 2;
    pub const EXEC: u32 = 4;
}

/// `mmap` flag bits.
pub mod flags {
    pub const SHARED: u64 = 0x01;
    pub const PRIVATE: u64 = 0x02;
    pub const FIXED: u64 = 0x10;
    pub const ANONYMOUS: u64 = 0x20;
    pub const NORESERVE: u64 = 0x4000;
    pub const STACK: u64 = 0x20000;
}

/// One contiguous mapping.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vma {
    pub start: u64,
    pub end: u64,
    pub prot: u32,
    /// A short label for `/proc/self/maps`.
    pub name: String,
    /// Where these bytes came from, when they came from somewhere nameable:
    /// the file, and the offset within it that `start` corresponds to.
    ///
    /// This is not for `/proc`. It is what lets two processes that map the
    /// same library at the same address agree that the *code* is the same,
    /// and so share the dispatch slots and the translations that went into
    /// them. Without it every `execve` starts a fresh page table and pays for
    /// libc again -- and in a browser tab, where the dispatch table cannot
    /// grow past ten million slots, paying twice is not merely slow.
    pub origin: Option<(String, u64)>,
}

impl Vma {
    /// A piece of this mapping, with the origin offset moved to match.
    fn slice(&self, start: u64, end: u64, prot: u32) -> Vma {
        Vma {
            start,
            end,
            prot,
            name: self.name.clone(),
            origin: self
                .origin
                .as_ref()
                .map(|(id, off)| (id.clone(), off + (start - self.start))),
        }
    }
}

/// The process's address space.
#[derive(Debug, Clone)]
pub struct MemoryMap {
    vmas: Vec<Vma>,
    brk_start: u64,
    brk_cur: u64,
    /// Next address the top-down allocator will try.
    mmap_next: u64,
}

pub fn page_align_up(v: u64) -> u64 {
    (v + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

pub fn page_align_down(v: u64) -> u64 {
    v & !(PAGE_SIZE - 1)
}

impl MemoryMap {
    pub fn new(brk_start: u64) -> MemoryMap {
        MemoryMap {
            vmas: Vec::new(),
            brk_start: page_align_up(brk_start),
            brk_cur: page_align_up(brk_start),
            mmap_next: MMAP_TOP,
        }
    }

    pub fn vmas(&self) -> &[Vma] {
        &self.vmas
    }

    pub fn brk_current(&self) -> u64 {
        self.brk_cur
    }

    /// Record a mapping made outside the allocator, such as an ELF segment or
    /// the initial stack.
    pub fn insert(&mut self, start: u64, end: u64, prot: u32, name: &str) {
        self.insert_from(start, end, prot, name, None);
    }

    /// As [`Self::insert`], recording where the bytes came from.
    pub fn insert_from(
        &mut self,
        start: u64,
        end: u64,
        prot: u32,
        name: &str,
        origin: Option<(String, u64)>,
    ) {
        let aligned = page_align_down(start);
        // The offset moves with the start: the caller names the offset of the
        // address it asked for, and rounding down to a page boundary walks
        // backwards into the file by the same amount.
        let origin = origin.map(|(id, off)| (id, off.saturating_sub(start - aligned)));
        self.vmas.push(Vma {
            start: aligned,
            end: page_align_up(end),
            prot,
            name: name.to_string(),
            origin,
        });
        self.vmas.sort_by_key(|v| v.start);
    }

    /// `brk(0)` reports the break; `brk(addr)` moves it and returns the new
    /// value. Linux returns the *current* break on failure rather than an
    /// error, which is what libc's allocator probes for.
    ///
    /// The second return value is the range the break just grew over, which
    /// the caller must zero. See [`Self::brk`]'s note on why that is not
    /// optional.
    pub fn brk_grow(&mut self, addr: u64) -> (u64, Option<(u64, u64)>) {
        if addr == 0 {
            return (self.brk_cur, None);
        }
        let new = page_align_up(addr);
        if new < self.brk_start || new >= MMAP_BOTTOM {
            return (self.brk_cur, None);
        }
        let grew = (new > self.brk_cur).then_some((self.brk_cur, new));
        if let Some((from, to)) = grew {
            self.insert(from, to, prot::READ | prot::WRITE, "[heap]");
        }
        self.brk_cur = new;
        (addr, grew)
    }

    /// Move the break without zeroing what it exposed.
    ///
    /// Only for callers with no guest memory to hand -- tests, mostly. A
    /// syscall must use [`Self::brk_grow`] and zero the range: Linux
    /// guarantees that memory reads as zero, and glibc's `calloc` *relies* on
    /// it, skipping its own memset for the part of a chunk that came from a
    /// fresh `sbrk`. Handing back the previous program's bytes instead means
    /// `calloc` returns uninitialised memory, and the first thing that
    /// notices is the dynamic loader finding a garbage field in a structure
    /// it just allocated.
    pub fn brk(&mut self, addr: u64) -> u64 {
        self.brk_grow(addr).0
    }

    /// Find and reserve a free range of `len` bytes.
    ///
    /// Allocation is top-down from [`MMAP_TOP`], matching Linux's default
    /// layout, so the heap growing up and mappings growing down stay apart.
    pub fn mmap(&mut self, addr: u64, len: u64, prot: u32, flags: u64, name: &str) -> SysResult {
        self.mmap_from(addr, len, prot, flags, name, None)
    }

    /// As [`Self::mmap`], recording the file and offset the bytes came from.
    pub fn mmap_from(
        &mut self,
        addr: u64,
        len: u64,
        prot: u32,
        flags: u64,
        name: &str,
        origin: Option<(String, u64)>,
    ) -> SysResult {
        if len == 0 {
            return Err(Errno::EINVAL);
        }
        let len = page_align_up(len);

        if flags & flags::FIXED != 0 {
            if !addr.is_multiple_of(PAGE_SIZE) {
                return Err(Errno::EINVAL);
            }
            let end = addr.checked_add(len).ok_or(Errno::ENOMEM)?;
            if end > GUEST_LIMIT {
                return Err(Errno::ENOMEM);
            }
            // MAP_FIXED replaces whatever was there.
            self.unmap(addr, len)?;
            self.insert_from(addr, end, prot, name, origin);
            return Ok(addr);
        }

        // An address hint is honoured when the range happens to be free.
        if addr != 0 && addr.is_multiple_of(PAGE_SIZE) {
            let end = addr.saturating_add(len);
            if end <= GUEST_LIMIT && self.is_free(addr, end) {
                self.insert_from(addr, end, prot, name, origin);
                return Ok(addr);
            }
        }

        let mut candidate = self.mmap_next;
        loop {
            if candidate < len || candidate - len < MMAP_BOTTOM {
                return Err(Errno::ENOMEM);
            }
            let start = page_align_down(candidate - len);
            if self.is_free(start, start + len) {
                self.mmap_next = start;
                self.insert_from(start, start + len, prot, name, origin);
                return Ok(start);
            }
            // Skip below the mapping that got in the way.
            match self
                .vmas
                .iter()
                .rev()
                .find(|v| v.start < start + len && v.end > start)
            {
                Some(v) => candidate = v.start,
                None => return Err(Errno::ENOMEM),
            }
        }
    }

    /// Grow or shrink an existing mapping, as `mremap` does.
    ///
    /// Returns the new base, which is the old one when the mapping could be
    /// extended in place and a fresh range otherwise. The caller moves the
    /// bytes: this map knows about ranges, not contents.
    ///
    /// musl's `realloc` calls this for anything mmap-backed, and a plain
    /// `ENOSYS` sends it down a malloc-copy-free path instead -- correct, but
    /// it copies every time a large buffer grows.
    pub fn mremap(
        &mut self,
        addr: u64,
        old_len: u64,
        new_len: u64,
        may_move: bool,
    ) -> Result<(u64, bool), Errno> {
        if !addr.is_multiple_of(PAGE_SIZE) || old_len == 0 || new_len == 0 {
            return Err(Errno::EINVAL);
        }
        let old_len = page_align_up(old_len);
        let new_len = page_align_up(new_len);
        // The whole range has to be one mapping, as Linux requires.
        let Some(v) = self
            .vmas
            .iter()
            .find(|v| v.start <= addr && v.end >= addr + old_len)
            .cloned()
        else {
            return Err(Errno::EFAULT);
        };
        if new_len <= old_len {
            if new_len < old_len {
                self.unmap(addr + new_len, old_len - new_len)?;
            }
            return Ok((addr, false));
        }
        let grown = addr + new_len;
        if grown <= GUEST_LIMIT && self.is_free(addr + old_len, grown) {
            self.insert(addr + old_len, grown, v.prot, &v.name);
            return Ok((addr, false));
        }
        if !may_move {
            return Err(Errno::ENOMEM);
        }
        let dst = self.mmap(0, new_len, v.prot, 0, &v.name)?;
        self.unmap(addr, old_len)?;
        Ok((dst, true))
    }

    fn is_free(&self, start: u64, end: u64) -> bool {
        !self.vmas.iter().any(|v| v.start < end && v.end > start)
    }

    /// Remove `[addr, addr+len)` from the map, splitting mappings as needed.
    pub fn unmap(&mut self, addr: u64, len: u64) -> Result<(), Errno> {
        if !addr.is_multiple_of(PAGE_SIZE) {
            return Err(Errno::EINVAL);
        }
        let len = page_align_up(len);
        let end = addr.checked_add(len).ok_or(Errno::EINVAL)?;
        let mut out = Vec::with_capacity(self.vmas.len() + 1);
        for v in std::mem::take(&mut self.vmas) {
            if v.end <= addr || v.start >= end {
                out.push(v);
                continue;
            }
            if v.start < addr {
                out.push(v.slice(v.start, addr, v.prot));
            }
            if v.end > end {
                out.push(v.slice(end, v.end, v.prot));
            }
        }
        out.sort_by_key(|v| v.start);
        self.vmas = out;
        Ok(())
    }

    /// Change protection over a range, splitting mappings as needed.
    pub fn protect(&mut self, addr: u64, len: u64, new_prot: u32) -> Result<(), Errno> {
        if !addr.is_multiple_of(PAGE_SIZE) {
            return Err(Errno::EINVAL);
        }
        let len = page_align_up(len);
        let end = addr.checked_add(len).ok_or(Errno::EINVAL)?;
        let mut out = Vec::with_capacity(self.vmas.len() + 2);
        let mut touched = false;
        for v in std::mem::take(&mut self.vmas) {
            if v.end <= addr || v.start >= end {
                out.push(v);
                continue;
            }
            touched = true;
            if v.start < addr {
                out.push(v.slice(v.start, addr, v.prot));
            }
            out.push(v.slice(v.start.max(addr), v.end.min(end), new_prot));
            if v.end > end {
                out.push(v.slice(end, v.end, v.prot));
            }
        }
        out.sort_by_key(|v| v.start);
        self.vmas = out;
        if touched {
            Ok(())
        } else {
            Err(Errno::ENOMEM)
        }
    }

    /// True when every page in the range is mapped with at least `need`.
    pub fn check_access(&self, addr: u64, len: u64, need: u32) -> bool {
        if len == 0 {
            return true;
        }
        let Some(end) = addr.checked_add(len) else {
            return false;
        };
        let mut cursor = addr;
        while cursor < end {
            match self
                .vmas
                .iter()
                .find(|v| v.start <= cursor && cursor < v.end)
            {
                Some(v) if v.prot & need == need => cursor = v.end,
                _ => return false,
            }
        }
        true
    }

    /// Render the map the way `/proc/self/maps` does.
    pub fn proc_maps(&self) -> String {
        let mut s = String::new();
        for v in &self.vmas {
            s.push_str(&format!(
                "{:08x}-{:08x} {}{}{}p 00000000 00:00 0 {}{}\n",
                v.start,
                v.end,
                if v.prot & prot::READ != 0 { 'r' } else { '-' },
                if v.prot & prot::WRITE != 0 { 'w' } else { '-' },
                if v.prot & prot::EXEC != 0 { 'x' } else { '-' },
                if v.name.is_empty() {
                    ""
                } else {
                    "                    "
                },
                v.name,
            ));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brk_grows_and_reports() {
        let mut m = MemoryMap::new(0x40_1000);
        assert_eq!(m.brk(0), 0x40_1000);
        assert_eq!(m.brk(0x40_2000), 0x40_2000);
        assert_eq!(m.brk(0), 0x40_2000);
        // Shrinking back is allowed.
        assert_eq!(m.brk(0x40_1000), 0x40_1000);
        // Below the start is refused, reported as "no change".
        assert_eq!(m.brk(0x10), 0x40_1000);
    }

    #[test]
    fn mmap_allocates_downwards_without_overlap() {
        let mut m = MemoryMap::new(0x40_1000);
        let a = m
            .mmap(0, 0x1000, prot::READ, flags::ANONYMOUS, "a")
            .unwrap();
        let b = m
            .mmap(0, 0x1000, prot::READ, flags::ANONYMOUS, "b")
            .unwrap();
        assert!(b < a, "expected top-down allocation, got {a:x} then {b:x}");
        assert!(a + 0x1000 <= MMAP_TOP);
        assert_eq!(a % PAGE_SIZE, 0);
    }

    #[test]
    fn unmap_splits_a_mapping_in_two() {
        let mut m = MemoryMap::new(0x40_1000);
        let a = m
            .mmap(0, 0x4000, prot::READ, flags::ANONYMOUS, "x")
            .unwrap();
        m.unmap(a + 0x1000, 0x1000).unwrap();
        let covering: Vec<_> = m.vmas().iter().filter(|v| v.name == "x").collect();
        assert_eq!(covering.len(), 2);
        assert_eq!(covering[0].end, a + 0x1000);
        assert_eq!(covering[1].start, a + 0x2000);
        assert!(!m.check_access(a + 0x1000, 0x1000, prot::READ));
        assert!(m.check_access(a, 0x1000, prot::READ));
    }

    #[test]
    fn protect_narrows_to_the_requested_range() {
        let mut m = MemoryMap::new(0x40_1000);
        let a = m
            .mmap(0, 0x3000, prot::READ | prot::WRITE, flags::ANONYMOUS, "x")
            .unwrap();
        m.protect(a + 0x1000, 0x1000, prot::READ | prot::EXEC)
            .unwrap();
        assert!(m.check_access(a + 0x1000, 0x1000, prot::EXEC));
        assert!(!m.check_access(a, 0x1000, prot::EXEC));
        assert!(!m.check_access(a + 0x2000, 0x1000, prot::EXEC));
    }

    #[test]
    fn fixed_mapping_replaces_whatever_was_there() {
        let mut m = MemoryMap::new(0x40_1000);
        let a = m
            .mmap(0, 0x2000, prot::READ, flags::ANONYMOUS, "x")
            .unwrap();
        let got = m
            .mmap(a, 0x1000, prot::WRITE, flags::ANONYMOUS | flags::FIXED, "y")
            .unwrap();
        assert_eq!(got, a);
        assert!(m.check_access(a, 0x1000, prot::WRITE));
    }

    #[test]
    fn check_access_spans_adjacent_mappings() {
        let mut m = MemoryMap::new(0x40_1000);
        m.insert(0x1000_0000, 0x1000_1000, prot::READ, "a");
        m.insert(0x1000_1000, 0x1000_2000, prot::READ, "b");
        assert!(m.check_access(0x1000_0800, 0x1000, prot::READ));
        assert!(!m.check_access(0x1000_1800, 0x1000, prot::READ));
    }
}
