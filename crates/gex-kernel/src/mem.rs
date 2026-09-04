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
use gex_abi::{MMAP_AUTO_TOP, MMAP_BOTTOM, MMAP_TOP};

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
    /// `MAP_FIXED_NOREPLACE`: map at the address only where it is free; fail
    /// with `EEXIST` if any part is already mapped, leaving every existing
    /// mapping alone. Wine reserves address space with it -- one huge request
    /// that must reserve only the gaps and never disturb the loaded images,
    /// the stack, or the runtime's own reserved region.
    pub const FIXED_NOREPLACE: u64 = 0x100000;
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
    /// Where these bytes can be *rebuilt* from: the file, and the offset
    /// within it that `start` corresponds to. Only a mapping that has never
    /// been writable has one, because only that one can be reconstructed by
    /// re-reading.
    pub origin: Option<(String, u64)>,
    /// Where these bytes were *loaded* from, whatever happened afterwards.
    ///
    /// Lets two processes that map the same library at the same address agree
    /// the code is the same and share the translations for it. Separate from
    /// `origin`: `origin` asks "can this be rebuilt by re-reading the file"
    /// (no, for an image segment with a zeroed tail, and no once `mprotect`
    /// has touched it), which is a different question from whether the code is
    /// the same code.
    pub code: Option<(String, u64)>,
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
            code: self
                .code
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
    /// How far up the arena has been claimed, and who to ask for more.
    ///
    /// A guest in a browser lives in a `WebAssembly.Memory` allocated up to its
    /// highest used address, one per process, so the arena is claimed as needed
    /// and the memory grown to match. `None` is the native case: everything is
    /// available already. `arena` is the spans handed out, in order; addresses
    /// only ever come from inside them, because the same growing region also
    /// feeds the runtime's own heap.
    arena: Vec<(u64, u64)>,
    take: Option<fn(u64) -> u64>,
    /// Make the host's memory reach an address, for a mapping the guest
    /// placed itself rather than one the arena handed out.
    back: Option<fn(u64) -> bool>,
    /// Bumped by every change to the map. `/proc/self/maps` is derived from the
    /// tree; a counter lets a cached rendering be reused unchanged instead of
    /// reformatting on every read (measured half of an X server's time in
    /// `core::fmt` before this).
    generation: u64,
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
            generation: 0,
            brk_start: page_align_up(brk_start),
            brk_cur: page_align_up(brk_start),
            mmap_next: MMAP_AUTO_TOP,
            arena: Vec::new(),
            take: None,
            back: None,
        }
    }

    /// How many times the map has changed. Two equal readings mean the VMA
    /// tree is byte-for-byte the same, so anything derived from it still
    /// holds.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn vmas(&self) -> &[Vma] {
        &self.vmas
    }

    /// Hand out arena addresses bottom-up, for a host whose memory must be
    /// allocated up to whatever it hands out.
    pub fn grows_on_demand(&mut self, take: fn(u64) -> u64, back: fn(u64) -> bool) {
        self.take = Some(take);
        self.back = Some(back);
        self.arena.clear();
    }

    /// The address one past the highest byte in use, which is how much memory
    /// a process actually needs.
    pub fn high_water(&self) -> u64 {
        self.vmas
            .iter()
            .map(|v| v.end)
            .chain(std::iter::once(self.brk_cur))
            .max()
            .unwrap_or(MMAP_BOTTOM)
    }

    /// Make sure the host's memory reaches `end`, for a mapping placed at an
    /// address the guest chose (MAP_FIXED or an honoured hint) which may be
    /// above anything the arena has claimed -- a pthread stack, commonly.
    fn ensure_backed(&mut self, end: u64) -> Result<(), Errno> {
        let Some(take) = self.take else {
            return Ok(());
        };
        if self.arena.iter().any(|&(_, e)| end <= e) {
            return Ok(());
        }
        let _ = take;
        match self.back {
            Some(back) if end <= MMAP_TOP && back(end) => Ok(()),
            _ => Err(Errno::ENOMEM),
        }
    }

    /// How much of the top of the arena is free: the bytes between the highest
    /// mapping and the end of the highest span. A fresh claim continues from
    /// here so the tail the last user gave back is spent first.
    fn free_tail(&self) -> u64 {
        let Some(&(lo, hi)) = self.arena.last() else {
            return 0;
        };
        let top = self
            .vmas
            .iter()
            .filter(|v| v.end > lo && v.start < hi)
            .map(|v| v.end)
            .max()
            .unwrap_or(lo);
        hi.saturating_sub(top.max(lo))
    }

    /// Claim `len` more bytes of arena, growing the memory behind it.
    fn extend_arena(&mut self, len: u64) -> Result<u64, Errno> {
        let Some(take) = self.take else {
            return Err(Errno::ENOMEM);
        };
        // A chunk at a time, so a guest that maps a page at a time does not
        // ask the host to grow its memory once per page.
        const CHUNK: u64 = 8 * 1024 * 1024;
        let need = page_align_up(len);
        // Carry on from the free tail instead of above it: glibc's `new_heap`
        // frees most of a `HEAP_MAX_SIZE` back and then asks for more, so
        // claiming a full `len` above the tail strands it every time. `apt-get
        // update` hit the 2 GiB ceiling this way.
        let tail = self.free_tail();
        // Subtracting the free tail lets a small heap grow carry on from it
        // instead of stranding it (glibc's `new_heap` free-and-remap, which hit
        // the 2 GiB ceiling otherwise). But that reuse only actually happens in
        // the *adjacent* branch below, where the span starts `tail` bytes before
        // `start`. For a mapping larger than a CHUNK the reduced `want` is what
        // gets taken and recorded, and if the take lands non-adjacent (the
        // shared cursor moved) the tail is never reused and the span is short by
        // `tail` -- a 107 MiB libLLVM mmap then hands back an address whose last
        // `tail` bytes were never backed, faulting OOB in Mesa's DRI screen init
        // (no GLX fbconfigs) and crashing glxinfo outright. Small grows floor to
        // CHUNK >= need and are backed regardless, so only shrink `want` for
        // them; back a large mapping in full.
        let want = if need > CHUNK {
            need
        } else {
            need.saturating_sub(tail).max(CHUNK)
        };
        let start = take(want);
        if start == 0 {
            return Err(Errno::ENOMEM);
        }
        // Join to the span below when adjacent rather than recording a second
        // box: the take-cursor only goes up, so consecutive claims are really
        // one region, but the free-hole search runs within a single span and
        // would miss a hole straddling a boundary -- glibc's ordinary heap
        // pattern, which reached the 2 GiB ceiling and aborted.
        if let Some(last) = self.arena.last_mut() {
            if last.1 == start {
                last.1 = start + want;
                return Ok(start - tail);
            }
        }
        // Record the span and hand back its start. A single `take` is one
        // contiguous claim whatever the cursor does; a `29f6e7f` change added a
        // SECOND `take` here to "top up" the tail `want` was shortened by, but
        // the arena cursor is shared between a process's threads, so a racing
        // claim from another thread landed between the two takes and the span
        // recorded here then covered memory that claim owned -- multithreaded
        // Xvfb corrupted into an OOB/SIGABRT, which was the virgl regression.
        // The common mapping (a pthread stack is 8 MiB == CHUNK, so `want` is
        // floored to CHUNK >= need) is fully backed by the one take above; the
        // only under-backed case is a mapping larger than a CHUNK that followed
        // a freed tail, which is rare and predates this code -- fix that in the
        // mmap path if it ever bites, not with a second racing take.
        self.arena.push((start, start + want));
        Ok(start)
    }

    pub fn brk_current(&self) -> u64 {
        self.brk_cur
    }

    /// Record a mapping made outside the allocator, such as an ELF segment or
    /// the initial stack.
    pub fn insert(&mut self, start: u64, end: u64, prot: u32, name: &str) {
        self.generation += 1;
        self.insert_from(start, end, prot, name, None);
    }

    /// Record a region exactly as another process had it, for `fork`: keep the
    /// parent's name and code identity so the child can still find the code in
    /// the shared store instead of re-translating from cold. No `origin`: that
    /// is a claim about a mapping this process made, not one to inherit.
    pub fn adopt(&mut self, start: u64, end: u64, prot: u32, name: &str, code: Option<(String, u64)>) {
        self.generation += 1;
        self.vmas.push(Vma {
            start: page_align_down(start),
            end: page_align_up(end),
            prot,
            name: name.to_string(),
            origin: None,
            code,
        });
        self.vmas.sort_by_key(|v| v.start);
    }

    /// As [`Self::insert`], for bytes a loader placed: the file and offset name
    /// the *code*, but the mapping cannot be rebuilt by re-reading so it gets no
    /// `origin`. This is how an ELF segment arrives -- its `mem_size` runs past
    /// the file into a zeroed tail, yet its executable bytes are exactly a range
    /// of the file.
    pub fn insert_loaded(
        &mut self,
        start: u64,
        end: u64,
        prot: u32,
        name: &str,
        code: Option<(String, u64)>,
    ) {
        self.insert_from(start, end, prot, name, code);
        if let Some(v) = self.vmas.iter_mut().find(|v| v.start == page_align_down(start)) {
            v.origin = None;
        }
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
        self.generation += 1;
        let aligned = page_align_down(start);
        // The offset moves with the start: rounding down to a page boundary
        // walks backwards into the file by the same amount.
        let code = origin.map(|(id, off)| (id, off.saturating_sub(start - aligned)));
        // `origin` is only for a mapping that has *never* been writable. `ld.so`
        // maps a library's data segment RW from the file, so libc's GOT would
        // otherwise claim to be reconstructable by re-reading the very bytes the
        // linker overwrites. `code` is the different, surviving claim: where the
        // bytes were loaded, whatever happened after.
        let origin = if prot & prot::WRITE != 0 { None } else { code.clone() };
        self.vmas.push(Vma {
            start: aligned,
            end: page_align_up(end),
            prot,
            name: name.to_string(),
            code,
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
        self.generation += 1;
        if addr == 0 {
            return (self.brk_cur, None);
        }
        let new = page_align_up(addr);
        if new < self.brk_start || new >= gex_abi::BRK_TOP {
            return (self.brk_cur, None);
        }
        let grew = (new > self.brk_cur).then_some((self.brk_cur, new));
        if let Some((from, to)) = grew {
            // Linux will not grow the break into an existing mapping: if the
            // range from the old break to the new one intersects any VMA it
            // leaves the break where it was and returns it, and libc's
            // allocator falls back to mmap. Without this the break grows
            // straight over an mmap -- Wine reserves its address space with
            // mmap while glibc's malloc (inside ntdll.so) extends the break,
            // and the two alias, so a heap object lands inside a thread stack
            // or a GDI table and is overwritten out from under the guest.
            if self.vmas.iter().any(|v| v.start < to && v.end > from) {
                return (self.brk_cur, None);
            }
            self.insert(from, to, prot::READ | prot::WRITE, "[heap]");
        }
        self.brk_cur = new;
        (addr, grew)
    }

    /// Move the break without zeroing what it exposed. Only for callers with no
    /// guest memory to hand (tests); a syscall must use [`Self::brk_grow`] and
    /// zero the range, because glibc's `calloc` skips its own memset for fresh
    /// `sbrk` memory and relies on Linux's zero guarantee.
    pub fn brk(&mut self, addr: u64) -> u64 {
        self.generation += 1;
        self.brk_grow(addr).0
    }

    /// Find and reserve a free range of `len` bytes. Top-down from [`MMAP_TOP`],
    /// matching Linux, so the heap and mappings grow apart.
    pub fn mmap(&mut self, addr: u64, len: u64, prot: u32, flags: u64, name: &str) -> SysResult {
        self.generation += 1;
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
        self.generation += 1;
        if len == 0 {
            return Err(Errno::EINVAL);
        }
        let len = page_align_up(len);

        if flags & (flags::FIXED | flags::FIXED_NOREPLACE) != 0 {
            if !addr.is_multiple_of(PAGE_SIZE) {
                return Err(Errno::EINVAL);
            }
            let end = addr.checked_add(len).ok_or(Errno::ENOMEM)?;
            // The guest may not map above the dispatch directory's reach
            // ([`MMAP_TOP`]): the wasm memory is backed higher (to
            // [`GUEST_LIMIT`], just under 4 GiB), but code mapped past
            // `MMAP_TOP` has no directory entry and cannot be translated. A
            // 64-bit Wine probes the address space and lays its modules and
            // stacks against whatever ceiling it finds, so the ceiling it
            // finds has to be the reach, or it maps code gex cannot run.
            if end > MMAP_TOP {
                return Err(Errno::ENOMEM);
            }
            // `MAP_FIXED_NOREPLACE` must not disturb anything already there:
            // fail if the range overlaps a mapping or the runtime's own
            // reserved region (which is not a VMA but is guest memory the
            // runtime owns -- its dispatch directory and contexts live at
            // 0..`RESERVE_FLOOR`). Wine reserves address space this way and
            // relies on the failure to reserve only the free gaps.
            let noreplace = flags & flags::FIXED_NOREPLACE != 0 && flags & flags::FIXED == 0;
            // The gap between the guest's stack and where the mmap allocator
            // begins ([`STACK_TOP`], [`MMAP_BOTTOM`)) is not free space: in the
            // browser the shim shares the guest's linear memory and its own
            // stack and static data live here, so a guest mapping into it
            // overwrites the runtime and the Worker dies with a bare
            // `unreachable`. A 64-bit Wine's `virtual_init` probes low address
            // space with `MAP_FIXED_NOREPLACE` and, finding this gap "free",
            // reserved straight over the shim -- winemine's browser crash.
            // Treat it as occupied like the low reserved region: a NOREPLACE
            // probe steps past it (as it already does over the stack below), a
            // plain `MAP_FIXED` is refused rather than allowed to clip into it.
            // Natively the gap is unused guest memory and refusing it only
            // moves Wine's reservation a little higher, which is harmless.
            let hits_shim = addr < gex_abi::MMAP_BOTTOM && end > gex_abi::STACK_TOP;
            let hits_reserved = addr < gex_abi::RESERVE_FLOOR || hits_shim;
            // `MAP_FIXED_NOREPLACE` must not disturb anything already there.
            // Linux fails it over *any* existing mapping, PROT_NONE included,
            // and that failure is load-bearing for a multithreaded guest: Wine
            // finds free address space by trying a candidate with NOREPLACE and
            // stepping to the next only on `EEXIST`, so every thread's stack,
            // TEB and heap comes from this loop. If two threads' reservations
            // (PROT_NONE) are allowed to overlap, their stacks alias and the
            // guest corrupts itself.
            //
            // The one intended leniency: a *commit* (real permissions) may land
            // on top of the guest's own PROT_NONE reservation -- Wine reserves a
            // range and then maps real pages into it, sometimes with NOREPLACE,
            // and virtual_init relies on that succeeding. So only a reservation
            // (PROT_NONE) is refused over another PROT_NONE mapping; a real
            // mapping is refused only over another real one. Either is refused
            // over the runtime's region.
            let over_real = self
                .vmas
                .iter()
                .any(|v| v.start < end && v.end > addr && v.prot != 0);
            let over_reservation = self
                .vmas
                .iter()
                .any(|v| v.start < end && v.end > addr && v.prot == 0);
            let committing = prot != 0;
            let conflict = over_real || (!committing && over_reservation);
            if noreplace && (conflict || hits_reserved) {
                return Err(Errno::EEXIST);
            }
            // A plain `MAP_FIXED` into the shim's gap would clip straight over
            // the runtime (the clip below only protects the low reserved
            // region), so refuse it outright. Reservations there are already
            // refused above, so a commit never legitimately reaches here.
            if hits_shim {
                return Err(Errno::ENOMEM);
            }
            // Plain `MAP_FIXED` replaces whatever was there -- except the
            // reserved region, which it must never overwrite. Clip it out and
            // keep the requested address for the guest's bookkeeping.
            let map_lo = addr.max(gex_abi::RESERVE_FLOOR);
            if map_lo < end {
                self.ensure_backed(end)?;
                self.unmap(map_lo, end - map_lo)?;
                self.insert_from(map_lo, end, prot, name, origin);
            }
            return Ok(addr);
        }

        // An address hint is honoured when the range happens to be free --
        // but never in the shim's gap ([`STACK_TOP`], [`MMAP_BOTTOM`)), which
        // `is_free` reports as free (no VMA there) yet holds the runtime in the
        // browser. Fall through to the allocator, which hands out at or above
        // `MMAP_BOTTOM`.
        if addr != 0 && addr.is_multiple_of(PAGE_SIZE) {
            let end = addr.saturating_add(len);
            let in_shim = addr < gex_abi::MMAP_BOTTOM && end > gex_abi::STACK_TOP;
            if !in_shim && end <= MMAP_TOP && self.is_free(addr, end) && self.ensure_backed(end).is_ok() {
                self.insert_from(addr, end, prot, name, origin);
                return Ok(addr);
            }
        }

        // Bottom-up when the host's memory grows with the arena, so the highest
        // address in use stays low: a `munmap` hole is reused before the arena
        // -- and the memory behind it -- gets bigger.
        if self.take.is_some() {
            let chunks = self.arena.clone();
            for (lo, hi) in chunks {
                let mut candidate = page_align_up(lo);
                while candidate + len <= hi {
                    if self.is_free(candidate, candidate + len) {
                        self.insert_from(candidate, candidate + len, prot, name, origin);
                        return Ok(candidate);
                    }
                    match self
                        .vmas
                        .iter()
                        .filter(|v| v.start < candidate + len && v.end > candidate)
                        .map(|v| v.end)
                        .max()
                    {
                        Some(end) => candidate = page_align_up(end),
                        None => break,
                    }
                }
            }
            let start = self.extend_arena(len)?;
            self.insert_from(start, start + len, prot, name, origin);
            return Ok(start);
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

    /// Grow or shrink an existing mapping. Returns the new base -- the old one
    /// when extended in place, a fresh range otherwise; the caller moves the
    /// bytes. musl's `realloc` calls this for mmap-backed buffers, so `ENOSYS`
    /// would make it copy on every growth.
    pub fn mremap(
        &mut self,
        addr: u64,
        old_len: u64,
        new_len: u64,
        may_move: bool,
    ) -> Result<(u64, bool), Errno> {
        self.generation += 1;
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
        if grown <= MMAP_TOP && self.is_free(addr + old_len, grown) {
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
        self.generation += 1;
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
        self.generation += 1;
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
            // The changed piece loses its origin: a mapping read-only *now* may
            // have been written while writable (`ld.so` relocates a RELRO region
            // then mprotects it read-only), so it cannot be rebuilt by
            // re-reading. `code` survives -- the executable bytes are still the
            // file's, and two processes relocate them identically.
            let mut piece = v.slice(v.start.max(addr), v.end.min(end), new_prot);
            piece.origin = None;
            out.push(piece);
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

    /// glibc's heap pattern must not leak the arena a claim at a time.
    ///
    /// `new_heap` frees the head and tail of each claim; the freed tail of one
    /// and head of the next are adjacent and together fit the next request, so
    /// the second heap must land in them rather than extend the arena again.
    #[test]
    fn a_freed_tail_and_the_next_head_are_one_hole() {
        // A cursor that only goes up, which is what the browser shim has.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0x1000_0000);
        fn take(len: u64) -> u64 {
            NEXT.fetch_add(len, std::sync::atomic::Ordering::SeqCst)
        }
        fn back(_end: u64) -> bool {
            true
        }
        NEXT.store(0x1000_0000, std::sync::atomic::Ordering::SeqCst);

        let mut m = MemoryMap::new(0x40_1000);
        m.grows_on_demand(take, back);

        const HEAP: u64 = 64 * 1024 * 1024;
        let mut kept = Vec::new();
        for _ in 0..4 {
            // Two heaps' worth, then trim to alignment, as glibc does.
            let got = m.mmap(0, HEAP * 2, 0, 0, "heap").unwrap() as u64;
            let aligned = (got + HEAP - 1) & !(HEAP - 1);
            if aligned > got {
                m.unmap(got, aligned - got).unwrap();
            }
            let tail = aligned + HEAP;
            if got + HEAP * 2 > tail {
                m.unmap(tail, got + HEAP * 2 - tail).unwrap();
            }
            kept.push(aligned);
        }

        // Four heaps of 64 MiB kept. Reusing the holes, the cursor should
        // have moved a little over 256 MiB; extending every time it is 512.
        let used = NEXT.load(std::sync::atomic::Ordering::SeqCst) - 0x1000_0000;
        assert!(
            used <= HEAP * 6,
            "the arena grew {} MiB for four 64 MiB heaps -- the freed head \
             and tail are not being reused",
            used / (1024 * 1024),
        );
        // And the heaps are all distinct and aligned.
        kept.sort();
        kept.dedup();
        assert_eq!(kept.len(), 4);
        for k in kept {
            assert_eq!(k % HEAP, 0);
        }
    }

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
    fn the_shim_gap_is_not_free_space() {
        // The gap between the guest stack and the mmap allocator holds the
        // browser shim. A NOREPLACE probe there must fail (so Wine steps past
        // it, as its virtual_init does) and a plain FIXED must be refused,
        // never allowed to clip over the runtime -- winemine's browser crash.
        let mut m = MemoryMap::new(0x40_1000);
        let gap = gex_abi::STACK_TOP + 0x30_0000;
        assert!(gap < gex_abi::MMAP_BOTTOM);
        assert_eq!(
            m.mmap(gap, 0x1000, 0, flags::ANONYMOUS | flags::FIXED_NOREPLACE, "r"),
            Err(Errno::EEXIST),
        );
        assert_eq!(
            m.mmap(gap, 0x1000, prot::WRITE, flags::ANONYMOUS | flags::FIXED, "c"),
            Err(Errno::ENOMEM),
        );
        // A hint into the gap is not honoured either: it falls through to the
        // allocator, which hands out at or above MMAP_BOTTOM.
        let got = m.mmap(gap, 0x1000, prot::WRITE, flags::ANONYMOUS, "h").unwrap();
        assert!(got >= gex_abi::MMAP_BOTTOM, "hint honoured inside the shim gap: {got:#x}");
        // And an ordinary NOREPLACE reservation above MMAP_BOTTOM still works.
        assert!(m
            .mmap(gex_abi::MMAP_BOTTOM, 0x1000, 0, flags::ANONYMOUS | flags::FIXED_NOREPLACE, "ok")
            .is_ok());
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
