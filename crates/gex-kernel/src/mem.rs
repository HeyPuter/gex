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
    /// `MAP_FIXED_NOREPLACE`: at exactly this address, but fail with `EEXIST`
    /// rather than replace anything already mapped there.
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
    ///
    /// It answers where the shared cursor stood before (`None` when the memory
    /// would not grow). The cursor only moves up, atomically, so the bytes
    /// between that and `end` are this map's alone, and are recorded as arena
    /// for later mappings to use.
    back: Option<fn(u64) -> Option<u64>>,
    /// Bumped by every change to the map. `/proc/self/maps` is derived from the
    /// tree; a counter lets a cached rendering be reused unchanged instead of
    /// reformatting on every read (measured half of an X server's time in
    /// `core::fmt` before this).
    generation: u64,
    /// An M64 process's high arena, `[HIGH_ARENA_BASE, LIMIT64)`
    /// (docs/mem64.md §3): non-executable mappings without a fixed address go
    /// there. `None` for every M32 process, whose map behaves exactly as it
    /// always has.
    high: Option<(u64, u64)>,
}

/// Bytes an M32 process asked for and could not be given for lack of address
/// space (not of memory): the run that sees it records the program for a
/// 64-bit memory next time (docs/mem64.md §5). Zero until it happens.
pub static ADDRESS_SPACE_SHORT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Note that an M32 process ran out of address space asking for `len` bytes.
pub fn note_address_space_short(len: u64) {
    ADDRESS_SPACE_SHORT.fetch_max(len.max(1), std::sync::atomic::Ordering::Relaxed);
}

/// Whether `[start, end)` touches guest memory the runtime owns but no VMA
/// records, so `is_free` would call it free:
///
/// - below [`gex_abi::DISPATCH_DIR_END`]: the thread contexts and the dispatch
///   directory. A guest mapping there wipes directory entries, and a hot block
///   then misses forever.
/// - `[STACK_TOP, MMAP_BOTTOM)`, the gap between the guest's stack and the
///   arena: in the browser the shim shares the guest's memory and keeps its
///   own stack and data there. Natively it is only unused, and refusing it
///   moves a guest's mapping elsewhere, which is harmless.
pub fn hits_runtime(start: u64, end: u64) -> bool {
    start < gex_abi::DISPATCH_DIR_END || (start < MMAP_BOTTOM && end > gex_abi::STACK_TOP)
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
            mmap_next: MMAP_TOP,
            arena: Vec::new(),
            take: None,
            back: None,
            // Every process of a host with a 64-bit guest memory has the
            // high arena (docs/mem64.md §3).
            high: gex_abi::mem64().then_some((gex_abi::HIGH_ARENA_BASE, gex_abi::LIMIT64)),
        }
    }

    /// Give this process a 64-bit memory's high arena (docs/mem64.md §3).
    pub fn set_high_arena(&mut self, lo: u64, hi: u64) {
        self.high = Some((lo, hi));
    }

    /// The highest address a mapping may end at, exclusive.
    fn limit(&self) -> u64 {
        self.high.map_or(GUEST_LIMIT, |(_, hi)| hi)
    }

    /// First fit, bottom-up, in the high arena: the browser grows memory to
    /// the highest address used, so low and dense is cheap.
    fn find_high(&self, len: u64) -> Option<u64> {
        let (lo, hi) = self.high?;
        let mut candidate = lo;
        loop {
            if candidate + len > hi {
                return None;
            }
            match self.vmas.iter().filter(|v| v.start < candidate + len && v.end > candidate).map(|v| v.end).max() {
                Some(end) => candidate = page_align_up(end),
                None => return Some(candidate),
            }
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
    pub fn grows_on_demand(&mut self, take: fn(u64) -> u64, back: fn(u64) -> Option<u64>) {
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

    /// Whether `[start, end)` lies in this map's own spans of the arena. The
    /// spans are sorted and disjoint (the cursor only climbs).
    fn in_arena(&self, start: u64, end: u64) -> bool {
        let mut at = start;
        for &(lo, hi) in &self.arena {
            if hi <= at {
                continue;
            }
            if lo > at {
                return false;
            }
            at = hi;
            if at >= end {
                return true;
            }
        }
        at >= end
    }

    /// Make `[start, end)` this process's to map, for a mapping at an address
    /// the guest chose (MAP_FIXED, an honoured hint, an mremap growing in
    /// place): backed, and not memory the runtime holds.
    ///
    /// Natively everything is. In the browser the arena above
    /// [`gex_abi::ARENA_BOTTOM`] is shared with the shim's own heap through
    /// one cursor, so a range there must lie in this map's spans, or above
    /// the cursor, which `back` then moves past it. Below `ARENA_BOTTOM` is
    /// the guest's fixed layout and the placement window, which the heap
    /// never takes from. A range the heap has a span in is ENOMEM: honouring
    /// it put a guest mapping over the shim's heap, and a program whose
    /// malloc passed mmap hints (musl built by zig) trapped in the shim after
    /// a hundred or so large allocations.
    fn claim(&mut self, start: u64, end: u64) -> Result<(), Errno> {
        if self.take.is_none() {
            return Ok(());
        }
        let floor = gex_abi::ARENA_BOTTOM;
        let ours = |m: &Self| end <= floor || m.in_arena(start.max(floor), end);
        if ours(self) {
            // Below the floor the memory may still need growing (the
            // placement window lies above what a process starts with).
            if end <= floor {
                if let Some(back) = self.back {
                    back(end).ok_or(Errno::ENOMEM)?;
                }
            }
            return Ok(());
        }
        let Some(back) = self.back.filter(|_| end <= MMAP_TOP) else {
            return Err(Errno::ENOMEM);
        };
        let Some(prev) = back(end) else {
            return Err(Errno::ENOMEM);
        };
        // What the cursor stepped over is backed now and nobody else's:
        // arena, so the next mapping lands in it rather than growing the
        // memory again. Without this a MAP_FIXED high up (a pinned library,
        // a thread stack placed by hint) stranded everything between the old
        // cursor and itself.
        let prev = prev.max(floor);
        if prev < end {
            match self.arena.last_mut() {
                Some(last) if last.1 == prev => last.1 = end,
                _ => self.arena.push((prev, end)),
            }
        }
        if ours(self) {
            Ok(())
        } else {
            Err(Errno::ENOMEM)
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
            if self.high.is_none() {
                note_address_space_short(len);
            }
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
        self.merge_adjacent();
    }

    /// Fold touching anonymous mappings with the same permissions and name
    /// into one, as Linux does. A program that reserves address space a
    /// chunk at a time otherwise leaves thousands of VMAs, which every
    /// placement scans and `/proc/self/maps` lists line by line (Wine
    /// re-parses that file with `sscanf`). Only mappings with no `code` or
    /// `origin` qualify: those identities are keyed on the exact span.
    fn merge_adjacent(&mut self) {
        let mut i = 0;
        while i + 1 < self.vmas.len() {
            let (a, b) = (&self.vmas[i], &self.vmas[i + 1]);
            if a.end == b.start
                && a.prot == b.prot
                && a.name == b.name
                && a.code.is_none()
                && b.code.is_none()
                && a.origin.is_none()
                && b.origin.is_none()
            {
                self.vmas[i].end = self.vmas[i + 1].end;
                self.vmas.remove(i + 1);
            } else {
                i += 1;
            }
        }
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
            // Linux will not grow the break into an existing mapping: it
            // leaves the break where it was and returns it, and libc's
            // allocator falls back to mmap. Without this the break grows
            // straight over an mmap and a heap object aliases whatever was
            // mapped there (Wine reserves address space just above the heap).
            // The heap's own records are not in the way: a shrink leaves the
            // pages above the break recorded, and growing back over them is
            // what the break is for.
            if self.vmas.iter().any(|v| v.start < to && v.end > from && v.name != "[heap]") {
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

    /// [`Self::mmap`] below the high arena whatever the protection: a
    /// mapping that will hold code (the dynamic loader's object reservation).
    pub fn mmap_low(&mut self, addr: u64, len: u64, prot: u32, flags: u64, name: &str) -> SysResult {
        let high = self.high.take();
        let r = self.mmap(addr, len, prot, flags, name);
        self.high = high;
        r
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
            if end > self.limit() {
                return Err(Errno::ENOMEM);
            }
            // Code above 4 GiB is beyond the dispatch directory (docs/mem64.md
            // §3), as for mprotect.
            if prot & prot::EXEC != 0 && self.high.is_some() && end > gex_abi::HIGH_ARENA_BASE {
                eprintln!("gex: an executable mapping at {addr:#x}..{end:#x}, above 4 GiB in a 64-bit memory: not supported yet (docs/mem64.md)");
                return Err(Errno::EACCES);
            }
            // `MAP_FIXED_NOREPLACE` (which wins over `MAP_FIXED`, as on
            // Linux) fails over any existing mapping, a PROT_NONE
            // reservation included: a program that probes for free address
            // space with it (Wine does, for every thread's stack) must not be
            // told that a taken range is free. The runtime's own regions
            // count as taken.
            if flags & flags::FIXED_NOREPLACE != 0 && (!self.is_free(addr, end) || hits_runtime(addr, end)) {
                return Err(Errno::EEXIST);
            }
            // A plain `MAP_FIXED` replaces the guest's mappings, never the
            // runtime's.
            if hits_runtime(addr, end) {
                return Err(Errno::ENOMEM);
            }
            // MAP_FIXED replaces whatever was there.
            self.claim(addr, end)?;
            self.unmap(addr, len)?;
            self.insert_from(addr, end, prot, name, origin);
            return Ok(addr);
        }

        // An address hint is honoured when the range happens to be free, and
        // is not the runtime's.
        if addr != 0 && addr.is_multiple_of(PAGE_SIZE) {
            let end = addr.saturating_add(len);
            if end <= self.limit()
                && self.is_free(addr, end)
                && !hits_runtime(addr, end)
                && self.claim(addr, end).is_ok()
            {
                self.insert_from(addr, end, prot, name, origin);
                return Ok(addr);
            }
        }

        // Anonymous data in an M64 process goes to the high arena; code
        // stays below MMAP_TOP, where the dispatch directory reaches. A file
        // mapping stays low whatever its protection: ld.so reserves a whole
        // library read-only and then maps its text executable inside it.
        if prot & prot::EXEC == 0 && origin.is_none() {
            if let Some(start) = self.find_high(len) {
                if let Some(back) = self.back {
                    back(start + len).ok_or(Errno::ENOMEM)?;
                }
                self.insert_from(start, start + len, prot, name, origin);
                return Ok(start);
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
                if self.high.is_none() {
                    note_address_space_short(len);
                }
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
        if grown <= self.limit()
            && self.is_free(addr + old_len, grown)
            && !hits_runtime(addr + old_len, grown)
            && self.claim(addr + old_len, grown).is_ok()
        {
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
        // Code in the high arena is beyond the dispatch directory's reach
        // (docs/mem64.md §3): refused, loudly, until a sparse page table
        // covers it.
        if new_prot & prot::EXEC != 0 && self.high.is_some() && end > gex_abi::HIGH_ARENA_BASE {
            eprintln!(
                "gex: mprotect makes {addr:#x}..{end:#x} executable, above 4 GiB in a 64-bit memory: \
                 not supported yet (docs/mem64.md)"
            );
            return Err(Errno::EACCES);
        }
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
        fn back(end: u64) -> Option<u64> {
            Some(NEXT.fetch_max(end, std::sync::atomic::Ordering::SeqCst))
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

    /// `MAP_FIXED_NOREPLACE` refuses any existing mapping, reservations
    /// included, and succeeds on free space.
    #[test]
    fn fixed_noreplace_refuses_any_existing_mapping() {
        let mut m = MemoryMap::new(0x40_1000);
        let anon = flags::ANONYMOUS | flags::PRIVATE;
        let base = 0x2000_0000;
        m.mmap(base, 0x4000, 0, anon | flags::FIXED, "reserved").unwrap();
        for prot in [0, prot::READ | prot::WRITE] {
            assert_eq!(
                m.mmap(base + 0x1000, 0x1000, prot, anon | flags::FIXED_NOREPLACE, "x"),
                Err(Errno::EEXIST),
                "NOREPLACE over a reservation, prot {prot}"
            );
            // And with MAP_FIXED as well: NOREPLACE wins.
            assert_eq!(
                m.mmap(base + 0x3000, 0x2000, prot, anon | flags::FIXED | flags::FIXED_NOREPLACE, "x"),
                Err(Errno::EEXIST),
            );
        }
        assert_eq!(m.mmap(base + 0x4000, 0x1000, 3, anon | flags::FIXED_NOREPLACE, "x"), Ok(base + 0x4000));
        // Plain MAP_FIXED still replaces the guest's own mapping.
        assert_eq!(m.mmap(base, 0x1000, 3, anon | flags::FIXED, "y"), Ok(base));
    }

    /// The contexts and dispatch directory below `DISPATCH_DIR_END`, and the
    /// gap the browser shim lives in above the stack, are not free space,
    /// though no VMA records them.
    #[test]
    fn the_runtime_regions_are_not_free_space() {
        let mut m = MemoryMap::new(0x40_1000);
        let anon = flags::ANONYMOUS | flags::PRIVATE;
        let gap = gex_abi::STACK_TOP + 0x30_0000;
        assert!(gap + 0x1000 <= MMAP_BOTTOM);
        for at in [0x10000, gex_abi::DISPATCH_DIR_END - 0x1000, gap] {
            assert_eq!(m.mmap(at, 0x1000, 0, anon | flags::FIXED_NOREPLACE, "r"), Err(Errno::EEXIST), "{at:#x}");
            assert_eq!(m.mmap(at, 0x1000, 3, anon | flags::FIXED, "f"), Err(Errno::ENOMEM), "{at:#x}");
            // A hint there is not honoured: the allocator places it instead.
            let got = m.mmap(at, 0x1000, 3, anon, "h").unwrap();
            assert!(!hits_runtime(got, got + 0x1000), "hint {at:#x} honoured at {got:#x}");
        }
        // A range that only straddles into the region is refused too.
        assert_eq!(
            m.mmap(gex_abi::STACK_TOP - 0x1000, 0x2000, 3, anon | flags::FIXED, "s"),
            Err(Errno::ENOMEM)
        );
        // Right above the directory and at the arena's bottom is fine.
        assert!(m.mmap(gex_abi::DISPATCH_DIR_END, 0x1000, 3, anon | flags::FIXED_NOREPLACE, "ok").is_ok());
        assert!(m.mmap(MMAP_BOTTOM, 0x1000, 3, anon | flags::FIXED_NOREPLACE, "ok").is_ok());
    }

    /// A MAP_FIXED above the arena moves the shared cursor past it; the
    /// bytes it stepped over are this map's, and the next mapping uses them
    /// instead of claiming more.
    #[test]
    fn memory_backed_for_a_fixed_mapping_is_reused() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        fn take(len: u64) -> u64 {
            NEXT.fetch_add(len, std::sync::atomic::Ordering::SeqCst)
        }
        fn back(end: u64) -> Option<u64> {
            Some(NEXT.fetch_max(end, std::sync::atomic::Ordering::SeqCst))
        }
        NEXT.store(0x1000_0000, std::sync::atomic::Ordering::SeqCst);
        let mut m = MemoryMap::new(0x40_1000);
        m.grows_on_demand(take, back);
        let anon = flags::ANONYMOUS | flags::PRIVATE;
        let first = m.mmap(0, 0x1000, 3, anon, "a").unwrap();
        // Someone else sharing the cursor (the shim's heap) claims a span.
        let theirs = take(0x10_0000);
        let high = 0x2000_0000;
        assert_eq!(m.mmap(high, 0x1000, 3, anon | flags::FIXED, "high"), Ok(high));
        let cursor = NEXT.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(cursor, high + 0x1000);
        // The next ordinary mapping comes from below the fixed one, and not
        // from the span the other user took.
        let next = m.mmap(0, 0x10_0000, 3, anon, "b").unwrap();
        assert!(next < high && next > first, "{next:#x}");
        assert!(next + 0x10_0000 <= theirs || next >= theirs + 0x10_0000, "{next:#x} overlaps {theirs:#x}");
        assert_eq!(NEXT.load(std::sync::atomic::Ordering::SeqCst), cursor, "the arena grew");
    }

    #[test]
    fn touching_anonymous_mappings_merge() {
        let mut m = MemoryMap::new(0x40_1000);
        let anon = flags::ANONYMOUS | flags::PRIVATE | flags::FIXED;
        let base = 0x2000_0000;
        for i in 0..8 {
            m.mmap(base + i * 0x1000, 0x1000, 0, anon, "").unwrap();
        }
        assert_eq!(m.vmas().len(), 1, "{:?}", m.vmas());
        assert_eq!((m.vmas()[0].start, m.vmas()[0].end), (base, base + 0x8000));
        // A different permission, or a file's bytes, stay separate.
        m.mmap(base + 0x8000, 0x1000, prot::READ, anon, "").unwrap();
        m.mmap_from(base + 0x9000, 0x1000, 0, anon, "", Some(("f".into(), 0))).unwrap();
        m.mmap(base + 0xa000, 0x1000, 0, anon, "").unwrap();
        assert_eq!(m.vmas().len(), 4);
        // Unmapping from the middle of a merged mapping still splits it.
        m.unmap(base + 0x2000, 0x1000).unwrap();
        assert_eq!(m.vmas().len(), 5);
        assert!(m.check_access(base + 0x8000, 0x1000, prot::READ));
    }

    /// The arena is shared with the shim's heap through one cursor. A hint,
    /// MAP_FIXED or in-place mremap into a span the heap took must not put a
    /// guest mapping over it: musl (as zig builds it) passes mmap hints, and
    /// a benchmark's hundredth large malloc trapped inside the browser shim.
    #[test]
    fn a_placed_mapping_never_lands_in_the_runtimes_heap() {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        fn take(len: u64) -> u64 {
            NEXT.fetch_add(len, std::sync::atomic::Ordering::SeqCst)
        }
        fn back(end: u64) -> Option<u64> {
            Some(NEXT.fetch_max(end, std::sync::atomic::Ordering::SeqCst))
        }
        NEXT.store(0x1000_0000, std::sync::atomic::Ordering::SeqCst);
        let mut m = MemoryMap::new(0x40_1000);
        m.grows_on_demand(take, back);
        let anon = flags::ANONYMOUS | flags::PRIVATE;
        let a = m.mmap(0, 0x10_0000, 3, anon, "a").unwrap();
        let a_top = a + 0x80_0000; // the kernel claims a CHUNK at a time
        // The heap takes the next span; the kernel's next claim lands above.
        let heap = take(0x100_0000);
        assert_eq!(heap, a_top);
        let b = m.mmap(0, 0x90_0000, 3, anon, "b").unwrap();
        assert!(b >= heap + 0x100_0000);
        let inside = heap + 0x1000;
        // A hint there is ignored; the mapping goes somewhere that is ours.
        let h = m.mmap(inside, 0x2000, 3, anon, "h").unwrap();
        assert!(h + 0x2000 <= heap || h >= heap + 0x100_0000, "hint honoured in the heap: {h:#x}");
        // MAP_FIXED there is refused.
        assert_eq!(m.mmap(inside, 0x2000, 3, anon | flags::FIXED, "f"), Err(Errno::ENOMEM));
        // A mapping at the top of the kernel's first span does not grow in
        // place into the heap span above it.
        let edge = m.mmap(a_top - 0x1000, 0x1000, 3, anon | flags::FIXED, "e").unwrap();
        let (moved_to, moved) = m.mremap(edge, 0x1000, 0x3000, true).unwrap();
        assert!(moved && (moved_to + 0x3000 <= heap || moved_to >= heap + 0x100_0000), "{moved_to:#x}");
        assert_eq!(m.mremap(moved_to, 0x3000, 0x3000, false), Ok((moved_to, false)));
        // Within its own spans a hint is still honoured.
        let own = a + 0x20_0000;
        assert_eq!(m.mmap(own, 0x1000, 3, anon, "o"), Ok(own));
        // Below the arena (the placement window) is the guest's.
        let placed = gex_abi::PLACE_BOTTOM + 0x1000;
        assert_eq!(m.mmap(placed, 0x1000, 3, anon | flags::FIXED, "p"), Ok(placed));
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

    /// The break stops at a mapping, as on Linux, but not at its own pages: a
    /// shrink leaves them recorded and growing back over them must work
    /// (glibc trims the heap top and grows it again all the time).
    #[test]
    fn brk_stops_at_a_mapping_but_not_at_its_own_heap() {
        let mut m = MemoryMap::new(0x40_1000);
        assert_eq!(m.brk(0x40_3000), 0x40_3000);
        assert_eq!(m.brk(0x40_1000), 0x40_1000);
        assert_eq!(m.brk(0x40_4000), 0x40_4000, "regrowing over the trimmed heap");
        m.mmap(0x40_6000, 0x1000, prot::READ, flags::FIXED | flags::ANONYMOUS, "reserved")
            .unwrap();
        assert_eq!(m.brk(0x40_8000), 0x40_4000, "the break must not grow over a mapping");
        assert_eq!(m.brk(0), 0x40_4000);
        assert_eq!(m.brk(0x40_6000), 0x40_6000, "up to the mapping is fine");
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

    /// docs/mem64.md §3: in an M64 process, data goes above 4 GiB, code stays
    /// below MMAP_TOP, and a high page cannot become code.
    #[test]
    fn a_64_bit_memory_puts_data_in_the_high_arena() {
        let mut m = MemoryMap::new(0x40_0000);
        m.set_high_arena(gex_abi::HIGH_ARENA_BASE, gex_abi::LIMIT64);
        // Unity's reservations: 512 MiB - 4 KiB with no access, twenty of them.
        let mut last = 0;
        for _ in 0..20 {
            let a = m.mmap(0, 0x1fff_f000, prot::NONE, 0, "res").unwrap();
            assert!(a >= gex_abi::HIGH_ARENA_BASE && a + 0x1fff_f000 <= gex_abi::LIMIT64);
            assert!(a > last);
            last = a;
        }
        let code = m.mmap(0, 0x10000, prot::READ | prot::WRITE | prot::EXEC, 0, "jit").unwrap();
        assert!(code < MMAP_TOP);
        assert_eq!(m.protect(last, 0x1000, prot::READ | prot::EXEC), Err(Errno::EACCES));
        assert!(m.protect(last, 0x1000, prot::READ | prot::WRITE).is_ok());
        // A hint or a fixed address up to the 64-bit limit is honoured.
        let fixed = 10u64 << 30;
        assert_eq!(m.mmap(fixed, 0x1000, prot::READ, flags::FIXED, "f"), Ok(fixed));
    }

    /// An M32 map is unchanged, and running out of address space is recorded
    /// for the next start (docs/mem64.md §5).
    #[test]
    fn a_32_bit_memory_records_running_out_of_address_space() {
        let mut m = MemoryMap::new(0x40_0000);
        let a = m.mmap(0, 0x1000, prot::READ, 0, "x").unwrap();
        assert!(a < MMAP_TOP);
        assert_eq!(m.mmap(1 << 33, 0x1000, prot::READ, flags::FIXED, "f"), Err(Errno::ENOMEM));
        while m.mmap(0, 0x1fff_f000, prot::NONE, 0, "res").is_ok() {}
        assert!(ADDRESS_SPACE_SHORT.load(std::sync::atomic::Ordering::Relaxed) >= 0x1fff_f000);
    }
}
