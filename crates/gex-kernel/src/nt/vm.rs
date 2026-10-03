//! Windows virtual memory (docs/nt-layer.md §1.2, §3.4).
//!
//! A region list in Windows' terms -- allocation base and protection, a state
//! and protection per page, and a type -- which is what
//! `MEMORY_BASIC_INFORMATION` reports and what reserve/commit semantics need.
//! Every change is mirrored into the Linux kernel's `MemoryMap`, which is what
//! the translator consults ("is this executable?") and what retires
//! translations of code that went away.

use super::{st, Call, Status, R};
use crate::mem::{self, MemoryMap};
use std::collections::BTreeMap;

pub const PAGE: u64 = 0x1000;
/// Windows' allocation granularity.
pub const GRANULE: u64 = 0x10000;
/// The allocator's range (docs/nt-layer.md §1.2). The top is Windows'
/// `MmHighestUserAddress + 1` for a 32-bit address space: the last 64 KiB
/// below 2 GiB are never handed out (and the browser shim's guest slice
/// ends there).
pub const ALLOC_LOW: u64 = 0x0040_0000;
pub const ALLOC_HIGH: u64 = 0x7fff_0000;

pub mod mem_flags {
    pub const COMMIT: u32 = 0x1000;
    pub const RESERVE: u32 = 0x2000;
    pub const DECOMMIT: u32 = 0x4000;
    pub const RELEASE: u32 = 0x8000;
    pub const FREE: u32 = 0x10000;
    pub const PRIVATE: u32 = 0x20000;
    pub const MAPPED: u32 = 0x40000;
    pub const RESET: u32 = 0x80000;
    pub const TOP_DOWN: u32 = 0x100000;
    pub const WRITE_WATCH: u32 = 0x200000;
    pub const PHYSICAL: u32 = 0x400000;
    pub const RESET_UNDO: u32 = 0x1000000;
    pub const LARGE_PAGES: u32 = 0x20000000;
    pub const IMAGE: u32 = 0x1000000;
}

pub mod page_prot {
    pub const NOACCESS: u32 = 0x01;
    pub const READONLY: u32 = 0x02;
    pub const READWRITE: u32 = 0x04;
    pub const WRITECOPY: u32 = 0x08;
    pub const EXECUTE: u32 = 0x10;
    pub const EXECUTE_READ: u32 = 0x20;
    pub const EXECUTE_READWRITE: u32 = 0x40;
    pub const EXECUTE_WRITECOPY: u32 = 0x80;
    pub const GUARD: u32 = 0x100;
    pub const NOCACHE: u32 = 0x200;
    pub const WRITECOMBINE: u32 = 0x400;
}

/// A Windows protection as Linux `PROT_*` bits.
pub fn linux_prot(p: u32) -> u32 {
    use page_prot::*;
    let base = p & 0xff;
    let mut r = 0;
    if base & (READONLY | READWRITE | WRITECOPY | EXECUTE_READ | EXECUTE_READWRITE | EXECUTE_WRITECOPY) != 0 {
        r |= mem::prot::READ;
    }
    if base & (READWRITE | WRITECOPY | EXECUTE_READWRITE | EXECUTE_WRITECOPY) != 0 {
        r |= mem::prot::WRITE;
    }
    if base & (EXECUTE | EXECUTE_READ | EXECUTE_READWRITE | EXECUTE_WRITECOPY) != 0 {
        r |= mem::prot::EXEC;
    }
    r
}

/// Whether `p` is one valid base protection plus modifiers.
pub fn valid_prot(p: u32) -> bool {
    let base = p & 0xff;
    base.count_ones() == 1 && p & !0x7ff == 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Private,
    Mapped,
    Image,
}

impl Kind {
    pub fn mem_type(self) -> u32 {
        match self {
            Kind::Private => mem_flags::PRIVATE,
            Kind::Mapped => mem_flags::MAPPED,
            Kind::Image => mem_flags::IMAGE,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Page {
    pub committed: bool,
    pub prot: u32,
}

#[derive(Clone, Debug)]
pub struct Region {
    pub base: u64,
    pub size: u64,
    pub alloc_prot: u32,
    pub kind: Kind,
    pub pages: Vec<Page>,
    /// For a view: the section it maps, so the view can be found again.
    pub name: String,
}

impl Region {
    pub fn end(&self) -> u64 {
        self.base + self.size
    }
    fn page(&self, a: u64) -> usize {
        ((a - self.base) / PAGE) as usize
    }
}

/// `MEMORY_BASIC_INFORMATION`.
#[derive(Clone, Copy, Debug)]
pub struct Mbi {
    pub base: u64,
    pub alloc_base: u64,
    pub alloc_prot: u32,
    pub size: u64,
    pub state: u32,
    pub prot: u32,
    pub mem_type: u32,
}

#[derive(Default, Debug)]
pub struct Vm {
    pub regions: BTreeMap<u64, Region>,
    /// Ranges kept for the staged layout's images (`section::Layout`):
    /// [`Vm::find_free`] avoids them while anything else fits, so a heap or
    /// another image does not take a core DLL's base before it loads.
    pub reserved: Vec<(u64, u64)>,
}

fn align_up(v: u64, a: u64) -> u64 {
    (v + a - 1) & !(a - 1)
}

impl Vm {
    pub fn new() -> Vm {
        Vm::default()
    }

    /// The region containing `a`.
    pub fn find(&self, a: u64) -> Option<&Region> {
        self.regions.range(..=a).next_back().map(|(_, r)| r).filter(|r| a < r.end())
    }

    fn find_mut(&mut self, a: u64) -> Option<&mut Region> {
        self.regions.range_mut(..=a).next_back().map(|(_, r)| r).filter(|r| a < r.end())
    }

    pub fn is_free(&self, start: u64, end: u64) -> bool {
        start >= PAGE
            && end <= gex_abi::GUEST_LIMIT
            && !self.regions.range(..end).any(|(_, r)| r.end() > start)
    }

    /// A free 64 KiB-aligned range of `size` bytes: bottom-up first fit, or
    /// top-down. `limit` caps the end (zero bits / 32-bit callers).
    pub fn find_free(&self, size: u64, top_down: bool, limit: u64) -> Option<u64> {
        if !self.reserved.is_empty() {
            if let Some(b) = self.find_free_in(size, top_down, limit, true) {
                return Some(b);
            }
        }
        self.find_free_in(size, top_down, limit, false)
    }

    /// [`Vm::find_free`], treating the layout's reserved ranges as taken
    /// when `avoid_reserved`.
    fn find_free_in(&self, size: u64, top_down: bool, limit: u64, avoid_reserved: bool) -> Option<u64> {
        let reserved: &[(u64, u64)] = if avoid_reserved { &self.reserved } else { &[] };
        // The start of the highest blocker overlapping [start, start+size),
        // and the end of the highest one.
        let below = |start: u64| -> Option<u64> {
            let r = self.regions.range(..start + size).next_back().filter(|(_, r)| r.end() > start).map(|(&b, _)| b);
            let q = reserved.iter().filter(|(b, e)| *b < start + size && *e > start).map(|(b, _)| *b).max();
            r.max(q)
        };
        let above = |at: u64| -> Option<u64> {
            let r = self.regions.range(..at + size).filter(|(_, r)| r.end() > at).map(|(_, r)| r.end()).max();
            let q = reserved.iter().filter(|(b, e)| *b < at + size && *e > at).map(|(_, e)| *e).max();
            r.max(q)
        };
        let high = ALLOC_HIGH.min(limit);
        if top_down {
            let mut end = high;
            loop {
                let start = (end.checked_sub(size)?) & !(GRANULE - 1);
                if start < ALLOC_LOW {
                    return None;
                }
                match below(start) {
                    None => return Some(start),
                    Some(b) => end = b,
                }
            }
        } else {
            let mut at = ALLOC_LOW;
            loop {
                if at + size > high {
                    return None;
                }
                match above(at) {
                    None => return Some(at),
                    Some(e) => at = align_up(e, GRANULE),
                }
            }
        }
    }

    /// Add a region. The caller has checked the range is free.
    pub fn insert(&mut self, r: Region) {
        self.regions.insert(r.base, r);
    }

    pub fn remove(&mut self, base: u64) -> Option<Region> {
        self.regions.remove(&base)
    }

    /// Reserve (and optionally commit) a new private region.
    pub fn reserve(&mut self, base: u64, size: u64, prot: u32, commit: bool, kind: Kind, name: &str) {
        let n = (size / PAGE) as usize;
        self.insert(Region {
            base,
            size,
            alloc_prot: prot,
            kind,
            pages: vec![Page { committed: commit, prot: if commit { prot } else { 0 } }; n],
            name: name.to_string(),
        });
    }

    /// Commit `[start, end)` inside one region. Returns the ranges that were
    /// not committed before (to be zeroed).
    pub fn commit(&mut self, start: u64, end: u64, prot: u32) -> R<Vec<(u64, u64)>> {
        let r = self.find_mut(start).ok_or(Status(st::MEMORY_NOT_ALLOCATED))?;
        if end > r.end() {
            return Err(Status(st::CONFLICTING_ADDRESSES));
        }
        let mut fresh = Vec::new();
        let (i0, i1) = (r.page(start), r.page(end - 1) + 1);
        for i in i0..i1 {
            let a = r.base + i as u64 * PAGE;
            if !r.pages[i].committed {
                match fresh.last_mut() {
                    Some((_, e)) if *e == a => *e = a + PAGE,
                    _ => fresh.push((a, a + PAGE)),
                }
            }
            r.pages[i] = Page { committed: true, prot };
        }
        Ok(fresh)
    }

    pub fn decommit(&mut self, start: u64, end: u64) -> R<()> {
        let r = self.find_mut(start).ok_or(Status(st::MEMORY_NOT_ALLOCATED))?;
        if end > r.end() {
            return Err(Status(st::UNABLE_TO_FREE_VM));
        }
        let (i0, i1) = (r.page(start), r.page(end - 1) + 1);
        for p in &mut r.pages[i0..i1] {
            *p = Page { committed: false, prot: 0 };
        }
        Ok(())
    }

    /// Change the protection of committed pages; the old protection of the
    /// first page.
    pub fn protect(&mut self, start: u64, end: u64, prot: u32) -> R<u32> {
        let r = self.find_mut(start).ok_or(Status(st::MEMORY_NOT_ALLOCATED))?;
        if end > r.end() {
            return Err(Status(st::CONFLICTING_ADDRESSES));
        }
        let (i0, i1) = (r.page(start), r.page(end - 1) + 1);
        if r.pages[i0..i1].iter().any(|p| !p.committed) {
            return Err(Status(st::NOT_COMMITTED));
        }
        let old = r.pages[i0].prot;
        for p in &mut r.pages[i0..i1] {
            p.prot = prot;
        }
        Ok(old)
    }

    pub fn query(&self, a: u64) -> Mbi {
        let page = a & !(PAGE - 1);
        match self.find(a) {
            Some(r) => {
                let i = r.page(page);
                let p = r.pages[i];
                let mut j = i;
                while j < r.pages.len() && r.pages[j] == p {
                    j += 1;
                }
                Mbi {
                    base: page,
                    alloc_base: r.base,
                    alloc_prot: r.alloc_prot,
                    size: (j - i) as u64 * PAGE,
                    state: if p.committed { mem_flags::COMMIT } else { mem_flags::RESERVE },
                    prot: if p.committed { p.prot } else { 0 },
                    mem_type: r.kind.mem_type(),
                }
            }
            None => {
                let next = self.regions.range(page..).next().map(|(&b, _)| b).unwrap_or(gex_abi::GUEST_LIMIT);
                Mbi {
                    base: page,
                    alloc_base: 0,
                    alloc_prot: 0,
                    size: next.saturating_sub(page),
                    state: mem_flags::FREE,
                    prot: page_prot::NOACCESS,
                    mem_type: 0,
                }
            }
        }
    }

    /// Mirror `[start, end)` into the Linux map: one mapping per run of
    /// committed pages with the same protection. Returns whether any code was
    /// there before, so the caller can retire translations.
    pub fn mirror(&self, lm: &mut MemoryMap, start: u64, end: u64) -> bool {
        let had_code = lm
            .vmas()
            .iter()
            .any(|v| v.prot & mem::prot::EXEC != 0 && v.start < end && v.end > start);
        let _ = lm.unmap(start, end - start);
        let mut a = start;
        while a < end {
            let Some(r) = self.find(a) else {
                a += PAGE;
                continue;
            };
            let i = r.page(a);
            let p = r.pages[i];
            let mut b = a + PAGE;
            while b < end.min(r.end()) && r.pages[r.page(b)] == p {
                b += PAGE;
            }
            if p.committed {
                lm.insert(a, b, linux_prot(p.prot), &r.name);
            }
            a = b;
        }
        had_code
    }
}

impl super::Process {
    /// Keep `[start, end)` out of the allocator for good, before boot: memory
    /// the host itself uses in an address space it shares with the guest
    /// (the browser shim's stack, data and heap). Reserved, never committed,
    /// so it is not in the Linux map and nothing the guest asks for lands on
    /// it.
    pub fn reserve_host(&self, start: u64, end: u64, name: &str) {
        let (start, end) = (start & !(GRANULE - 1), align_up(end, GRANULE));
        let mut st = self.st.lock().unwrap();
        debug_assert!(st.vm.is_free(start, end));
        st.vm.reserve(start, end - start, page_prot::NOACCESS, false, Kind::Private, name);
    }
}

// ---------------------------------------------------------------------------
// Syscalls
// ---------------------------------------------------------------------------

const CURRENT_PROCESS: u64 = u64::MAX;

/// Only the current process is supported; other processes don't exist yet.
pub fn check_process(c: &Call<'_>, h: u64) -> R<()> {
    let h32 = h as u32;
    if h == CURRENT_PROCESS || h32 == u32::MAX {
        return Ok(());
    }
    // A real handle to our own process is fine too.
    let st = c.p.st.lock().unwrap();
    if let Some(e) = st.handles.get(h32) {
        if matches!(e.obj, super::object::Obj::Process) {
            return Ok(());
        }
    }
    drop(st);
    Err(c.unimplemented(&format!("{} on another process (handle {h:#x})", c.name())))
}

/// Apply a change to `[start, end)` in the Linux map and zero fresh pages.
pub fn after_change(c: &mut Call<'_>, start: u64, end: u64, fresh: &[(u64, u64)]) -> R<()> {
    for &(a, b) in fresh {
        c.g.zero(a, (b - a) as usize)?;
    }
    let st = c.p.st.lock().unwrap();
    let mut k = c.kernel();
    if st.vm.mirror(&mut k.mem, start, end) {
        k.retired_code.push((start, end));
    }
    Ok(())
}

/// The highest address an allocation may end at, from `ZeroBits`.
fn zero_bits_limit(_c: &Call<'_>, zero_bits: u64) -> u64 {
    let top = ALLOC_HIGH;
    match zero_bits {
        0 => top,
        1..=31 => (1u64 << (32 - zero_bits)).min(top),
        // A mask, as Windows allows on 64-bit.
        m => (m + 1).next_power_of_two().min(top),
    }
}

/// Core of NtAllocateVirtualMemory(Ex).
/// [`allocate_inner`], traced under `GEX_NT_TRACE`: what was asked (address,
/// size, type, protection) and what was given, which is how a program's
/// reservation pattern is read off a run.
fn allocate(c: &mut Call<'_>, addr_p: u64, limit: u64, size_p: u64, typ: u32, prot: u32) -> R {
    if !c.p.cfg.trace {
        return allocate_inner(c, addr_p, limit, size_p, typ, prot);
    }
    let (want, size) = (c.g.ptr(addr_p).unwrap_or(0), c.g.ptr(size_p).unwrap_or(0));
    let r = allocate_inner(c, addr_p, limit, size_p, typ, prot);
    let got = if r.is_ok() { c.g.ptr(addr_p).unwrap_or(0) } else { 0 };
    let got_size = if r.is_ok() { c.g.ptr(size_p).unwrap_or(0) } else { 0 };
    crate::nt_log!(
        "[nt:{:x}]   alloc want={want:#x} size={size:#x} type={typ:#x} prot={prot:#x} -> {} at {got:#x}+{got_size:#x}",
        c.t.tid,
        match &r { Ok(_) => "ok".to_string(), Err(e) => format!("{:#010x}", e.0) }
    );
    r
}

fn allocate_inner(c: &mut Call<'_>, addr_p: u64, limit: u64, size_p: u64, typ: u32, prot: u32) -> R {
    use mem_flags::*;
    let addr = c.g.ptr(addr_p)?;
    let size = c.g.ptr(size_p)?;
    if typ & !(COMMIT | RESERVE | TOP_DOWN | RESET | RESET_UNDO | WRITE_WATCH | LARGE_PAGES | PHYSICAL) != 0 {
        return Err(Status(st::INVALID_PARAMETER));
    }
    if typ & (RESET | RESET_UNDO) != 0 {
        // MEM_RESET only says the contents may be discarded: keeping them is
        // allowed.
        return Ok(st::SUCCESS);
    }
    if typ & (COMMIT | RESERVE) == 0 || size == 0 {
        return Err(Status(st::INVALID_PARAMETER));
    }
    if !valid_prot(prot) {
        return Err(Status(st::INVALID_PAGE_PROTECTION));
    }
    if typ & (LARGE_PAGES | PHYSICAL) != 0 {
        return Err(c.unimplemented("NtAllocateVirtualMemory with MEM_LARGE_PAGES or MEM_PHYSICAL"));
    }
    let top_down = typ & TOP_DOWN != 0;
    let mut stl = c.p.st.lock().unwrap();
    let (start, end, fresh);
    if addr == 0 || typ & RESERVE != 0 {
        // A new reservation.
        let (b, s) = if addr == 0 {
            let s = align_up(size, PAGE);
            let b = stl.vm.find_free(align_up(s, GRANULE), top_down, limit).ok_or(Status(st::NO_MEMORY))?;
            (b, s)
        } else {
            let b = addr & !(GRANULE - 1);
            let s = align_up(addr + size, PAGE) - b;
            if !stl.vm.is_free(b, b + s) {
                return Err(Status(st::CONFLICTING_ADDRESSES));
            }
            (b, s)
        };
        let commit = typ & COMMIT != 0;
        stl.vm.reserve(b, s, prot, commit, Kind::Private, "[nt]");
        start = b;
        end = b + s;
        fresh = if commit { vec![(b, b + s)] } else { Vec::new() };
    } else {
        // Commit inside an existing reservation.
        let b = addr & !(PAGE - 1);
        let e = align_up(addr + size, PAGE);
        fresh = stl.vm.commit(b, e, prot)?;
        start = b;
        end = e;
    }
    drop(stl);
    after_change(c, start, end, &fresh)?;
    c.g.wptr(addr_p, start)?;
    c.g.wptr(size_p, end - start)?;
    Ok(st::SUCCESS)
}

/// `NtAllocateVirtualMemory(process, *addr, zero_bits, *size, type, prot)`.
pub fn nt_allocate_virtual_memory(c: &mut Call<'_>) -> R {
    check_process(c, c.a(0))?;
    let limit = zero_bits_limit(c, c.a(2));
    allocate(c, c.a(1), limit, c.a(3), c.a32(4), c.a32(5))
}

/// `NtAllocateVirtualMemoryEx(process, *addr, *size, type, prot, params, count)`.
pub fn nt_allocate_virtual_memory_ex(c: &mut Call<'_>) -> R {
    check_process(c, c.a(0))?;
    if c.a32(6) != 0 {
        return Err(c.unimplemented("NtAllocateVirtualMemoryEx with extended parameters"));
    }
    let limit = zero_bits_limit(c, 0);
    allocate(c, c.a(1), limit, c.a(2), c.a32(3), c.a32(4))
}

/// `NtFreeVirtualMemory(process, *addr, *size, type)`.
pub fn nt_free_virtual_memory(c: &mut Call<'_>) -> R {
    use mem_flags::*;
    check_process(c, c.a(0))?;
    let (addr_p, size_p, typ) = (c.a(1), c.a(2), c.a32(3));
    let addr = c.g.ptr(addr_p)?;
    let size = c.g.ptr(size_p)?;
    let mut stl = c.p.st.lock().unwrap();
    let r = stl.vm.find(addr).cloned().ok_or(Status(st::MEMORY_NOT_ALLOCATED))?;
    if r.kind != Kind::Private {
        return Err(Status(st::UNABLE_TO_DELETE_SECTION));
    }
    let (start, end) = match typ {
        RELEASE => {
            if size == 0 {
                if addr != r.base {
                    return Err(Status(st::FREE_VM_NOT_AT_BASE));
                }
                stl.vm.remove(r.base);
                (r.base, r.end())
            } else {
                // Releasing part of a region splits it, as Windows allows
                // for whole placeholder-free ranges; Wine does the same.
                let s = addr & !(PAGE - 1);
                let e = align_up(addr + size, PAGE);
                if e > r.end() {
                    return Err(Status(st::UNABLE_TO_FREE_VM));
                }
                let mut whole = stl.vm.remove(r.base).unwrap();
                let tail_pages = whole.pages.split_off(((e - whole.base) / PAGE) as usize);
                let mid_start = ((s - whole.base) / PAGE) as usize;
                whole.pages.truncate(mid_start);
                if !tail_pages.is_empty() {
                    stl.vm.insert(Region {
                        base: e,
                        size: tail_pages.len() as u64 * PAGE,
                        pages: tail_pages,
                        ..whole.clone()
                    });
                }
                if !whole.pages.is_empty() {
                    whole.size = whole.pages.len() as u64 * PAGE;
                    stl.vm.insert(whole);
                }
                (s, e)
            }
        }
        DECOMMIT => {
            let (s, e) = if size == 0 {
                (r.base, r.end())
            } else {
                (addr & !(PAGE - 1), align_up(addr + size, PAGE))
            };
            stl.vm.decommit(s, e)?;
            (s, e)
        }
        _ => return Err(Status(st::INVALID_PARAMETER)),
    };
    drop(stl);
    after_change(c, start, end, &[])?;
    c.g.wptr(addr_p, start)?;
    c.g.wptr(size_p, end - start)?;
    Ok(st::SUCCESS)
}

/// `NtProtectVirtualMemory(process, *addr, *size, new, *old)`.
pub fn nt_protect_virtual_memory(c: &mut Call<'_>) -> R {
    check_process(c, c.a(0))?;
    let (addr_p, size_p, prot, old_p) = (c.a(1), c.a(2), c.a32(3), c.a(4));
    if !valid_prot(prot) {
        return Err(Status(st::INVALID_PAGE_PROTECTION));
    }
    let addr = c.g.ptr(addr_p)?;
    let size = c.g.ptr(size_p)?;
    let start = addr & !(PAGE - 1);
    let end = align_up(addr + size.max(1), PAGE);
    let old = c.p.st.lock().unwrap().vm.protect(start, end, prot)?;
    after_change(c, start, end, &[])?;
    c.g.wptr(addr_p, start)?;
    c.g.wptr(size_p, end - start)?;
    c.g.out32(old_p, old)?;
    Ok(st::SUCCESS)
}

/// `NtQueryVirtualMemory(process, addr, class, buffer, len, *ret_len)`.
pub fn nt_query_virtual_memory(c: &mut Call<'_>) -> R {
    let (addr, class, buf, len, ret_p) = (c.a(1), c.a32(2), c.a(3), c.a(4), c.a(5));
    match class {
        // MemoryBasicInformation
        0 => {
            check_process(c, c.a(0))?;
            let size = super::off!(c.p.abi, memory_basic_information::SIZEOF);
            if len < size {
                return Err(Status(st::INFO_LENGTH_MISMATCH));
            }
            if addr >= gex_abi::GUEST_LIMIT {
                return Err(Status(st::INVALID_PARAMETER));
            }
            let m = c.p.st.lock().unwrap().vm.query(addr);
            write_mbi(c, buf, &m)?;
            c.g.out_ptr(ret_p, size)?;
            Ok(st::SUCCESS)
        }
        // MemoryMappedFilenameInformation
        2 => super::section::query_mapped_filename(c, addr, buf, len, ret_p),
        // MemoryImageInformation
        6 => super::section::query_image_information(c, addr, buf, len, ret_p),
        // MemoryWineLoadUnixLib, MemoryWineLoadUnixLibByName
        1000 | 1002 => super::unixcall::load_unix_lib(c, class, addr, buf, len),
        _ => Err(c.unimplemented(&format!("NtQueryVirtualMemory class {class}"))),
    }
}

pub fn write_mbi(c: &mut Call<'_>, buf: u64, m: &Mbi) -> R<()> {
    use super::off;
    let abi = c.p.abi;
    let g = &mut c.g;
    g.zero(buf, off!(abi, memory_basic_information::SIZEOF) as usize)?;
    g.wptr(buf + off!(abi, memory_basic_information::BASEADDRESS), m.base)?;
    g.wptr(buf + off!(abi, memory_basic_information::ALLOCATIONBASE), m.alloc_base)?;
    g.w32(buf + off!(abi, memory_basic_information::ALLOCATIONPROTECT), m.alloc_prot)?;
    g.wptr(buf + off!(abi, memory_basic_information::REGIONSIZE), m.size)?;
    g.w32(buf + off!(abi, memory_basic_information::STATE), m.state)?;
    g.w32(buf + off!(abi, memory_basic_information::PROTECT), m.prot)?;
    g.w32(buf + off!(abi, memory_basic_information::TYPE), m.mem_type)?;
    Ok(())
}

/// `NtFlushInstructionCache(process, addr, size)`: translations of
/// self-modified code are retired by gex's own write guards, so this only
/// retires the range explicitly for code rewritten while not executable.
pub fn nt_flush_instruction_cache(c: &mut Call<'_>) -> R {
    let (addr, size) = (c.a(1), c.a(2));
    if addr != 0 && size != 0 {
        let start = addr & !(PAGE - 1);
        let end = align_up(addr + size, PAGE);
        c.kernel().retired_code.push((start, end));
    }
    Ok(st::SUCCESS)
}

/// `NtReadVirtualMemory(process, addr, buffer, size, *bytes_read)`, for the
/// current process only.
pub fn nt_read_virtual_memory(c: &mut Call<'_>) -> R {
    check_process(c, c.a(0))?;
    let (addr, buf, size, ret) = (c.a(1), c.a(2), c.a(3) as usize, c.a(4));
    let bytes = c.g.bytes(addr, size).map_err(|_| Status(st::PARTIAL_COPY))?;
    c.g.write(buf, &bytes)?;
    c.g.out_ptr(ret, size as u64)?;
    Ok(st::SUCCESS)
}
