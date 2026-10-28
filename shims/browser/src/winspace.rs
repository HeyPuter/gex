//! A Windows program's address space in a page: Wine's views and the
//! shim's own heap in one wasm memory.
//!
//! Two layouts. The compact one ([`wine32_windows_space`]) leaves the arena as
//! a Linux process's and keeps an island for executables and pinned DLLs;
//! the split ([`split_windows_space`]) grows the memory to 2 GiB at start
//! and moves the shim's heap above it. Only the shared-memory build runs a
//! Windows program.

#[cfg(feature = "shared-memory")]
pub(crate) use imp::*;
#[cfg(not(feature = "shared-memory"))]
pub(crate) use stub::*;

#[cfg(not(feature = "shared-memory"))]
mod stub {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// As the shared build's: `MMAP_TOP`, or all of the memory for a
    /// process that translates nothing ([`raise_native_ceiling`]).
    static ARENA_CEILING: AtomicU64 = AtomicU64::new(gex_abi::MMAP_TOP);

    pub(crate) fn arena_ceiling() -> u64 {
        ARENA_CEILING.load(Ordering::Relaxed)
    }

    pub(crate) fn raise_native_ceiling() {
        ARENA_CEILING.store(gex_abi::GUEST_MEM_BYTES, Ordering::Relaxed);
    }

    pub(crate) fn windows_space() -> u64 {
        0
    }

    pub(crate) fn wine32_windows_space() -> u64 {
        0
    }

    pub(crate) fn set_wine_split(_on: bool) {}

    pub(crate) fn wine32_split() -> bool {
        false
    }

    pub(crate) fn wine32_space() -> Result<u64, String> {
        Ok(wine32_windows_space())
    }

    pub(crate) fn wine32_compact() -> bool {
        false
    }

    /// No exe slot to keep in this build.
    pub(crate) const WINE32_ISLAND: (u64, u64) = (0, 0);

    pub(crate) fn wine32_kuser() {}

    pub(crate) fn wine64_compact() {}

    pub(crate) fn wine64_compact_on() -> bool {
        false
    }

    pub(crate) fn adopt_windows_space(_lo: u64) -> Result<(), String> {
        Err("a Windows address space needs the shared-memory build".into())
    }
}

#[cfg(feature = "shared-memory")]
mod imp {
    use crate::{grow_memory_to, warn_line, ARENA_NEXT};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    /// Where the shim's heap may climb to. A Linux process's arena ends at
    /// `MMAP_TOP` (the dispatch directory's reach); an NT process owns
    /// everything below 2 GiB, so its heap lives above it.
    static ARENA_CEILING: AtomicU64 = AtomicU64::new(gex_abi::MMAP_TOP);

    pub(crate) fn arena_ceiling() -> u64 {
        ARENA_CEILING.load(Ordering::Relaxed)
    }

    /// A process that translates nothing (no placement window) names no
    /// code in the dispatch directory, so its arena may use all of the
    /// memory rather than stop at the directory's reach.
    pub(crate) fn raise_native_ceiling() {
        ARENA_CEILING.store(gex_abi::GUEST_MEM_BYTES, Ordering::Relaxed);
    }

    /// The bottom of the shim's heap in an NT process: the top of the
    /// Windows user address space and above.
    pub(crate) const NT_HEAP_BASE: u64 = 0x8000_0000;

    /// The 2 GiB split Windows address space: everything below 2 GiB is
    /// Windows', so the memory is grown to all of it now -- KUSER_SHARED_DATA
    /// at 0x7ffe0000, the PEB and TEBs top-down reach its top at once --
    /// rather than allocation by allocation. The shim's own stack and data
    /// sit at fixed addresses inside it, and so does whatever its heap has
    /// taken so far: those stay the shim's for good, and from here on the
    /// heap climbs above 2 GiB, where no Windows address is. Returns where
    /// the space the host left starts (it ends at 2 GiB).
    ///
    /// Native Wine (32-bit Wine) does not take this: see [`wine32_windows_space`].
    pub(crate) fn split_windows_space() -> Result<u64, String> {
        // Split already (an `execve` in a 32-bit Wine process, or a fork's child):
        // the same line again. The heap is above it by now, so measuring
        // afresh would put the line at 2 GiB and leave no space at all.
        let lo = WINDOWS_LO.load(Ordering::Relaxed);
        if lo != 0 {
            return Ok(lo);
        }
        if !grow_memory_to(NT_HEAP_BASE) {
            return Err("the engine will not grow this memory to 2 GiB, which a Windows process needs".into());
        }
        ARENA_CEILING.store(gex_abi::GUEST_MEM_BYTES, Ordering::Relaxed);
        let taken = ARENA_NEXT.fetch_max(NT_HEAP_BASE, Ordering::AcqRel);
        let host_hi = taken.max(gex_kernel::mem::arena_bottom()).min(NT_HEAP_BASE);
        crate::memmap::claim(crate::memmap::Kind::Guest, 0, host_hi, NT_HEAP_BASE, "Windows address space");
        WINDOWS_LO.store(host_hi, Ordering::Relaxed);
        Ok(host_hi)
    }

    /// A native Wine process (32-bit Wine): marked as Windows-hosting, and nothing
    /// grown or claimed. Wine's own allocator takes its views from the
    /// kernel's arena bottom-up, through the same cursor as the shim's heap,
    /// as any Linux process's mappings do, so the memory reaches only as
    /// high as the process uses (each wasm memory counts its whole size
    /// towards the tab's data segment, RLIMIT_DATA, whether touched or not).
    /// What Windows pins high (KUSER_SHARED_DATA, an image at its preferred
    /// base) still grows it, when it is mapped. Returns the arena's floor,
    /// [`WINE32_COMPACT_LO`], which only says "32-bit Wine" to the kernel and to a forked
    /// child.
    pub(crate) fn wine32_windows_space() -> u64 {
        let lo = WINDOWS_LO.load(Ordering::Relaxed);
        if lo != 0 {
            return lo;
        }
        WINE32_COMPACT.store(true, Ordering::Relaxed);
        WINDOWS_LO.store(WINE32_COMPACT_LO, Ordering::Relaxed);
        keep_exe_slot();
        WINE32_COMPACT_LO
    }

    /// `?winesplit=1` (a site that asks): 32-bit Wine takes the 2 GiB split
    /// instead of the compact space, as it did before that existed: the
    /// memory grown to 2 GiB at start and the shim's heap above it, so
    /// everything below 2 GiB is Wine's. Costs the tab 2 GiB of data segment
    /// per process; for a game whose views and the heap do not fit together
    /// under 2 GiB (one needed 1.5 GiB of guest mappings and 164 MiB of heap).
    static WINE32_SPLIT: AtomicBool = AtomicBool::new(false);

    pub(crate) fn set_wine_split(on: bool) {
        WINE32_SPLIT.store(on, Ordering::Relaxed);
    }

    /// Whether this is a 32-bit Wine process that took the split (`?winesplit=1`):
    /// everything below 2 GiB is Wine's, the shim's heap above it.
    pub(crate) fn wine32_split() -> bool {
        WINE32_SPLIT.load(Ordering::Relaxed) && !WINE32_COMPACT.load(Ordering::Relaxed) && WINDOWS_LO.load(Ordering::Relaxed) != 0
    }

    /// A 32-bit Wine process's address space: the compact one, or the split when the
    /// site asked (`set_wine_split`). Where the Windows space starts (the
    /// split's line, or [`WINE32_COMPACT_LO`]).
    pub(crate) fn wine32_space() -> Result<u64, String> {
        if WINE32_SPLIT.load(Ordering::Relaxed) {
            split_windows_space().map(|lo| lo & !WINE32_KUSER_BIT)
        } else {
            Ok(wine32_windows_space())
        }
    }

    /// Where Windows executables want to be that the arena would reach:
    /// 0x10000000, Wine's own services, svchost, explorer, rundll32 and
    /// wineboot, and MSVC's default DLL base (16 MiB; the largest image a
    /// bundle has there is 9.4 MiB). A relocated native-wasm skeleton
    /// executable did not run (wineboot at 0x320000: a table call into a
    /// provider never installed). Above it, [`DLL_BAND`]: the DLLs a bundle
    /// pins (its prefix's dll-bases, from wine32/dll-bases.py), at the addresses
    /// its units were built for. Both are kept out of the arena's way as one
    /// island, which grows nothing until something is mapped there, as Wine's
    /// reservation keeps 0x10000-0x05000000 for executables at 0x400000 and
    /// 0x01000000. Low, so that a process's memory reaches about 0.3 GiB for
    /// its images, not the 1.5 GiB of the old band at 0x60000000.
    fn keep_exe_slot() {
        // Not if the heap is there already (then images relocate, and every
        // pinned DLL misses its unit: said once, it is a rare, costly event).
        if !crate::memmap::claim_island(EXE_SLOT, DLL_BAND_END, &crate::ARENA_NEXT, "Windows executables (0x10000000) and pinned DLLs (0x11000000)") {
            warn_line(&format!(
                "gex: wine32: the exe slot and DLL band [{EXE_SLOT:#x}, {DLL_BAND_END:#x}) are not kept (arena cursor {:#x}); pinned images will relocate\n",
                ARENA_NEXT.load(Ordering::Acquire)
            ));
        }
    }
    const EXE_SLOT: u64 = 0x1000_0000;
    /// [`keep_exe_slot`]'s island, `[EXE_SLOT, DLL_BAND_END)`: where only a
    /// mapping that names its address goes (`wine32_address_space`).
    pub(crate) const WINE32_ISLAND: (u64, u64) = (EXE_SLOT, DLL_BAND_END);
    /// The pinned DLLs' band, [0x11000000, 0x20000000):
    /// tools/wine-wasm/wine32/dll-bases.py lays them out from its start and
    /// asserts they end below its end.
    const DLL_BAND: u64 = 0x1100_0000;
    const DLL_BAND_END: u64 = 0x2000_0000;
    const _: () = assert!(EXE_SLOT + (16 << 20) == DLL_BAND && DLL_BAND < DLL_BAND_END);

    /// What a 32-bit Wine process's `windows_space` says: no line (1 is never one, a
    /// split's line being page-aligned), only "Windows lives here".
    pub(crate) const WINE32_COMPACT_LO: u64 = 1;

    /// With [`WINE32_COMPACT_LO`]: this 32-bit Wine maps KUSER_SHARED_DATA at
    /// `gex_abi::WINE32_KUSER` (its header's `FLAG_KUSER_WINE32`), so a forked child
    /// folds as its parent did.
    const WINE32_KUSER_BIT: u64 = 2;

    /// This process's Wine has KUSER_SHARED_DATA at 32-bit Wine's address: fold
    /// Windows' constant addresses to it in everything translated from now
    /// on, and fault by name on any other access to the old page and above
    /// (`gex_abi::KUSER_GUARD`).
    pub(crate) fn wine32_kuser() {
        gex_abi::set_kuser_fold(true);
        gex_xlate::set_guard_limit(gex_abi::KUSER_GUARD);
        WINDOWS_LO.fetch_or(WINE32_KUSER_BIT, Ordering::Relaxed);
    }

    /// With [`WINE32_COMPACT_LO`] and [`WINE32_KUSER_BIT`]: a 64-bit Wine process
    /// in the compact space ([`wine64_compact`]), so a forked child folds the
    /// entry page and places the PE data window as its parent did.
    const WINE64_COMPACT_BIT: u64 = 4;

    /// A 64-bit Wine process in a wasm32 memory that takes 32-bit Wine's compact
    /// space instead of the split: KUSER_SHARED_DATA and the entry page after
    /// it folded to [`gex_abi::WINE32_KUSER`], so nothing pins the memory's
    /// length near 2 GiB. Its ntdll sees the folded address
    /// (`gex.kuser_address`) and allocates bottom-up. After [`wine32_windows_space`].
    pub(crate) fn wine64_compact() {
        wine32_kuser();
        gex_abi::set_kuser_fold_entry(true);
        WINDOWS_LO.fetch_or(WINE64_COMPACT_BIT, Ordering::Relaxed);
    }

    /// Whether this is a 64-bit Wine process in the compact space ([`wine64_compact`]).
    pub(crate) fn wine64_compact_on() -> bool {
        WINE32_COMPACT.load(Ordering::Relaxed) && WINDOWS_LO.load(Ordering::Relaxed) & WINE64_COMPACT_BIT != 0
    }

    /// Whether this process's Windows address space is 32-bit Wine's compact one
    /// (no split; see [`wine32_windows_space`]).
    static WINE32_COMPACT: AtomicBool = AtomicBool::new(false);

    pub(crate) fn wine32_compact() -> bool {
        WINE32_COMPACT.load(Ordering::Relaxed)
    }

    /// Where this process's Windows address space starts, or 0 when it has
    /// none. A fork hands it to the child (`gex_windows_space`).
    static WINDOWS_LO: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn windows_space() -> u64 {
        WINDOWS_LO.load(Ordering::Relaxed)
    }

    /// A forked child's half of the parent's split, before the child's heap
    /// takes its first span: the 2 GiB split at the parent's line, or
    /// 32-bit Wine's compact space (the child's memory already covers what the
    /// parent's image used).
    pub(crate) fn adopt_windows_space(lo: u64) -> Result<(), String> {
        if lo & !(WINE32_KUSER_BIT | WINE64_COMPACT_BIT) == WINE32_COMPACT_LO {
            WINE32_COMPACT.store(true, Ordering::Relaxed);
            WINDOWS_LO.store(WINE32_COMPACT_LO, Ordering::Relaxed);
            keep_exe_slot();
            if lo & WINE64_COMPACT_BIT != 0 {
                wine64_compact();
            } else if lo & WINE32_KUSER_BIT != 0 {
                wine32_kuser();
            }
            return Ok(());
        }
        // A split's line, with 32-bit Wine's KUSER bit when the parent is a 32-bit Wine process
        // that took the split (`?winesplit=1`) and folds KUSER_SHARED_DATA.
        let kuser = lo & WINE32_KUSER_BIT != 0;
        let lo = lo & !WINE32_KUSER_BIT;
        if !grow_memory_to(NT_HEAP_BASE) {
            return Err("the engine will not grow this memory to 2 GiB".into());
        }
        ARENA_CEILING.store(gex_abi::GUEST_MEM_BYTES, Ordering::Relaxed);
        ARENA_NEXT.fetch_max(NT_HEAP_BASE, Ordering::AcqRel);
        crate::memmap::claim(crate::memmap::Kind::Guest, 0, lo, NT_HEAP_BASE, "Windows address space");
        WINDOWS_LO.store(lo, Ordering::Relaxed);
        if kuser {
            wine32_kuser();
        }
        Ok(())
    }
}
