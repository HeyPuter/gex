//! The runtime translator in the host: misses, the module log every Worker installs from, and the executable ranges
//! and intrinsics translation consults.
//!
//! A miss translates under one lock, appends the module to the log, installs it on this Worker and publishes its
//! dispatch-directory entries. Other Workers install it when they next miss: on a page whose directory entry they
//! read before they had the module (the table guard sends them to the miss stub), or on the slot the module fills.

use gex_abi::DISPATCH_TABLES;
use gex_emit::module::{DISPATCH_MISS_LEAVE, DISPATCH_MISS_REDISPATCH};
use gex_xlate::{GuestWindow, Intrinsic, PageTable};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

/// One module in the log, as the JS library reads it.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Row {
    /// The module's bytes, never freed.
    pub bytes: u32,
    pub len: u32,
    /// How long each dispatch table must be before the module is instantiated.
    pub need: [u32; DISPATCH_TABLES],
}

/// How many modules the log holds.
const LOG_CAP: usize = 1 << 16;

/// Modules in the log. Stored (Release) after the row it counts is written.
pub static LOG_LEN: AtomicU32 = AtomicU32::new(0);

/// The rows, allocated once.
static ROWS: AtomicU32 = AtomicU32::new(0);

/// Blocks one miss translates at most: the next miss picks up where it stopped.
const LIFT_MAX: usize = 2048;

/// Misses in a row at one address before it is called a livelock.
const LIVELOCK: u32 = 64;

struct Jit {
    page_table: PageTable,
    known: HashSet<u64>,
    /// Executable ranges, `[lo, hi)`, sorted and disjoint.
    exec: Vec<(u64, u64)>,
    intrinsics: HashMap<u64, Intrinsic>,
}

static JIT: Mutex<Option<Jit>> = Mutex::new(None);

pub fn init() {
    let mut g = JIT.lock().unwrap();
    if g.is_some() {
        return;
    }
    let rows = vec![Row::default(); LOG_CAP].leak();
    ROWS.store(rows.as_ptr() as usize as u32, Ordering::Relaxed);
    let thunks = crate::layout::thunk_range();
    *g = Some(Jit { page_table: PageTable::new(), known: HashSet::new(), exec: vec![thunks], intrinsics: HashMap::new() });
}

pub fn rows() -> u32 {
    ROWS.load(Ordering::Relaxed)
}

fn with<R>(f: impl FnOnce(&mut Jit) -> R) -> R {
    f(JIT.lock().unwrap().as_mut().expect("gex_embed_init first"))
}

/// Mark `[lo, hi)` executable.
pub fn map_exec(lo: u64, hi: u64) {
    with(|j| {
        j.exec.push((lo, hi));
        j.exec.sort_unstable();
        // Merge what touches.
        let mut out: Vec<(u64, u64)> = Vec::with_capacity(j.exec.len());
        for &(a, b) in &j.exec {
            match out.last_mut() {
                Some(last) if a <= last.1 => last.1 = last.1.max(b),
                _ => out.push((a, b)),
            }
        }
        j.exec = out;
    })
}

/// `[lo, hi)` is no longer executable: drop it from the ranges, and retire what was translated there (its pages'
/// directory entries go to zero, so every Worker's next branch there misses).
pub fn unmap_exec(lo: u64, hi: u64) {
    with(|j| {
        let mut out = Vec::with_capacity(j.exec.len() + 1);
        for &(a, b) in &j.exec {
            if b <= lo || a >= hi {
                out.push((a, b));
                continue;
            }
            if a < lo {
                out.push((a, lo));
            }
            if b > hi {
                out.push((hi, b));
            }
        }
        j.exec = out;
        j.known.retain(|&a| a < lo || a >= hi);
        j.intrinsics.retain(|&a, _| a < lo || a >= hi);
        let dir = gex_abi::dispatch_dir();
        for p in lo / gex_abi::DISPATCH_PAGE..hi.div_ceil(gex_abi::DISPATCH_PAGE) {
            j.page_table.forget(p);
            // SAFETY: the directory's entry for page `p`, in the hosted block.
            unsafe { &*((dir + p * 4) as usize as *const AtomicU32) }.store(0, Ordering::Release);
        }
    })
}

/// Serve `pc` with `k` instead of translating what is there. Before anything reaches `pc`.
pub fn add_intrinsic(pc: u64, k: Intrinsic) {
    with(|j| {
        assert!(!j.known.contains(&pc), "an intrinsic at {pc:#x}, already translated");
        j.intrinsics.insert(pc, k);
    })
}

fn range_of(exec: &[(u64, u64)], a: u64) -> Option<(u64, u64)> {
    let i = exec.partition_point(|r| r.1 <= a);
    exec.get(i).copied().filter(|r| r.0 <= a)
}

/// Install what this Worker lacks; whether there was anything.
pub fn sync() -> bool {
    // SAFETY: the JS library's, with the descriptor it expects.
    unsafe { crate::host::js::gexjs_sync(crate::desc()) != 0 }
}

thread_local! {
    static LAST_MISS: Cell<(u64, u32)> = const { Cell::new((0, 0)) };
}

/// `gex.dispatch_miss`: `pc` has no translation this Worker can reach.
pub fn miss(pc: u64) -> i32 {
    if crate::layout::pad_depth(pc).is_some() {
        return DISPATCH_MISS_LEAVE;
    }
    LAST_MISS.with(|l| {
        let (last, n) = l.get();
        let n = if last == pc { n + 1 } else { 0 };
        if n >= LIVELOCK {
            crate::host::fatal(&format!("{pc:#x} misses again after being translated ({n} times)"));
        }
        l.set((pc, n));
    });
    // Behind: what this Worker lacks may be what it missed on.
    if sync() {
        return DISPATCH_MISS_REDISPATCH;
    }
    let mut g = JIT.lock().unwrap();
    let Jit { page_table, known, exec, intrinsics } = g.as_mut().expect("gex_embed_init first");
    if known.contains(&pc) {
        // Another Worker translated it after this one synced.
        drop(g);
        sync();
        return DISPATCH_MISS_REDISPATCH;
    }
    let Some((lo, hi)) = range_of(exec, pc) else {
        drop(g);
        let t = crate::thread::current();
        crate::host::fatal(&format!("a jump to {pc:#x}, which is not executable\n{}", crate::diag::backtrace(t.ctx)));
    };
    // The mapping being translated, and nothing else: a slice of the host's memory from zero can't exist (it would
    // start at a null pointer, and be longer than isize::MAX).
    // SAFETY: an executable range is a live allocation of the host's; translation only reads it.
    let window = GuestWindow { bytes: unsafe { core::slice::from_raw_parts(lo as usize as *const u8, (hi - lo) as usize) }, base: lo };
    let exec_now = exec.clone();
    let (mut unit, stats) = gex_xlate::translate_runtime_in(
        window,
        pc,
        &|a| range_of(&exec_now, a).is_some_and(|r| a + gex_abi::INST_ALIGN <= r.1),
        &|a| known.contains(&a),
        std::mem::take(page_table),
        Default::default(),
        LIFT_MAX,
        true,
        intrinsics,
    );
    if stats.blocks == 0 {
        crate::host::fatal(&format!("nothing translatable at {pc:#x}"));
    }
    for (a, _) in &unit.exported_blocks {
        known.insert(*a);
    }
    *page_table = std::mem::take(&mut unit.page_table);
    // The pages this unit numbered, and any page it covers whose directory entry is still zero.
    let dir = gex_abi::dispatch_dir();
    let entry = |p: u64| unsafe { &*((dir + p * 4) as usize as *const AtomicU32) };
    let mut dirs = unit.dir_entries.clone();
    for &(p, b) in &unit.covered {
        if p < gex_abi::dispatch_max_pages() && entry(p).load(Ordering::Relaxed) == 0 && !dirs.iter().any(|d| d.0 == p) {
            dirs.push((p, b));
        }
    }
    let n = LOG_LEN.load(Ordering::Relaxed) as usize;
    assert!(n < LOG_CAP, "the module log is full");
    let mut row = Row { len: unit.wasm.len() as u32, ..Row::default() };
    for (t, need) in row.need.iter_mut().enumerate() {
        *need = page_table.table_need(t) as u32;
    }
    row.bytes = unit.wasm.leak().as_ptr() as usize as u32;
    // SAFETY: the row past the end, which no reader looks at until the length says so.
    unsafe { *(rows() as usize as *mut Row).add(n) = row };
    LOG_LEN.store(n as u32 + 1, Ordering::Release);
    // Installed here before the directory names it, so this Worker never needs the guard for its own work.
    sync();
    for (p, b) in dirs {
        entry(p).store(b, Ordering::Release);
    }
    DISPATCH_MISS_REDISPATCH
}
