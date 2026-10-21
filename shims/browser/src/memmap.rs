//! Who owns each part of the shared memory, written down and checked.
//!
//! The guest, the shim and every Worker live in one linear memory with no
//! protection between them, so a range handed to two owners is silent
//! corruption: a Worker's first call running on wasm-ld's default stack wrote
//! into a game's .text for months. Every region is registered here with
//! its owner as it is created -- the fixed layout once, then each heap span,
//! each Worker's stack and thread-local block, each arena span the guest's
//! mappings take, the Windows address space at NT boot -- and a region that
//! overlaps another owner's stops the run, naming both. Guest regions may
//! overlap each other (the guest remaps its own memory); nothing else may
//! overlap anything.
//!
//! No allocation anywhere here: the heap's refill path registers its spans.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Kind {
    /// The guest's: image, stack, mappings, the Windows address space.
    Guest = 1,
    /// The runtime's fixed low region: contexts, scratch words, directory.
    Reserved = 2,
    /// The shim's data and bss (and the first instance's thread-locals).
    ShimData = 3,
    /// Bootstrap stacks for `gex_worker_init` (shims/browser/js/gex.mjs).
    BootStacks = 4,
    /// A span of the shim's heap (talc).
    Heap = 5,
    /// A Worker's shim stack (owner: its slot).
    Stack = 6,
    /// A Worker's thread-local block (owner: its slot).
    Tls = 7,
    /// Memory grown by someone other than the arena (std's `System`
    /// allocator): the arena steps over it.
    Foreign = 8,
    /// The guest's, above the arena's cursor: a mapping placed at a fixed
    /// address there (W3's compact space: KUSER_SHARED_DATA, an image at its
    /// base), or one a forked child inherited. The guest's like `Guest`,
    /// and the arena steps over it as over `Foreign`, rather than the cursor
    /// jumping past it and everything between staying unused.
    Island = 9,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Guest => "guest",
            Kind::Reserved => "reserved",
            Kind::ShimData => "shim data",
            Kind::BootStacks => "bootstrap stacks",
            Kind::Heap => "shim heap",
            Kind::Stack => "worker stack",
            Kind::Tls => "worker tls",
            Kind::Foreign => "foreign",
            Kind::Island => "guest island",
        }
    }
    fn from(v: u64) -> Kind {
        match v {
            1 => Kind::Guest,
            2 => Kind::Reserved,
            3 => Kind::ShimData,
            4 => Kind::BootStacks,
            5 => Kind::Heap,
            6 => Kind::Stack,
            7 => Kind::Tls,
            9 => Kind::Island,
            _ => Kind::Foreign,
        }
    }
}

const N: usize = 4096;

/// `[start, end)` and `kind << 32 | owner` per entry; `len` entries used.
struct Table {
    start: [AtomicU64; N],
    end: [AtomicU64; N],
    tag: [AtomicU64; N],
    /// What each entry is, a static string's address and length.
    what: [AtomicU64; N],
    len: AtomicU32,
    lock: AtomicU32,
    full_said: AtomicU32,
}

static T: Table = Table {
    start: [const { AtomicU64::new(0) }; N],
    end: [const { AtomicU64::new(0) }; N],
    tag: [const { AtomicU64::new(0) }; N],
    what: [const { AtomicU64::new(0) }; N],
    len: AtomicU32::new(0),
    lock: AtomicU32::new(0),
    full_said: AtomicU32::new(0),
};

struct Held;
impl Held {
    fn take() -> Held {
        while T.lock.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
            std::hint::spin_loop();
        }
        Held
    }
}
impl Drop for Held {
    fn drop(&mut self) {
        T.lock.store(0, Ordering::Release);
    }
}

fn what_of(i: usize) -> &'static str {
    let v = T.what[i].load(Ordering::Relaxed);
    let (p, l) = ((v >> 32) as usize, (v & 0xffff_ffff) as usize);
    if l == 0 {
        return "";
    }
    // SAFETY: stored from a `&'static str` in `claim`.
    unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(p as *const u8, l)) }
}

/// A line into a stack buffer and out through `warn`, allocating nothing.
struct Line([u8; 600], usize);
impl std::fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let n = s.len().min(self.0.len() - self.1);
        self.0[self.1..self.1 + n].copy_from_slice(&s.as_bytes()[..n]);
        self.1 += n;
        Ok(())
    }
}
fn say(args: std::fmt::Arguments<'_>) {
    use std::fmt::Write as _;
    let mut l = Line([0; 600], 0);
    let _ = l.write_fmt(args);
    let _ = l.write_str("\n");
    // SAFETY: the import reads the bytes and keeps none.
    unsafe { crate::warn(l.0.as_ptr() as u32, l.1 as u32) };
}

/// Register `[start, end)` for `kind` (`owner`: a Worker slot, or 0), as
/// `what`. Overlapping another owner's region stops the run.
pub fn claim(kind: Kind, owner: u32, start: u64, end: u64, what: &'static str) {
    if end <= start {
        return;
    }
    let _h = Held::take();
    claim_locked(kind, owner, start, end, what);
}

impl Kind {
    fn guests(self) -> bool {
        matches!(self, Kind::Guest | Kind::Island)
    }
}

fn claim_locked(kind: Kind, owner: u32, start: u64, end: u64, what: &'static str) {
    let tag = (kind as u64) << 32 | owner as u64;
    let n = T.len.load(Ordering::Relaxed) as usize;
    for i in 0..n {
        let (s, e) = (T.start[i].load(Ordering::Relaxed), T.end[i].load(Ordering::Relaxed));
        if s < end && start < e {
            let other = T.tag[i].load(Ordering::Relaxed);
            let ok = kind.guests() && Kind::from(other >> 32).guests();
            if !ok {
                say(format_args!(
                    "[gex] BUG: memory map overlap: {} {:#x}..{:#x} ({} of {}) overlaps {} {:#x}..{:#x} ({} of {})",
                    kind.name(),
                    start,
                    end,
                    what,
                    owner,
                    Kind::from(other >> 32).name(),
                    s,
                    e,
                    what_of(i),
                    other & 0xffff_ffff
                ));
                print_locked(n);
                crate::warch::unreachable();
            }
        }
    }
    // Extend the last entry when this one continues it.
    if n > 0
        && T.tag[n - 1].load(Ordering::Relaxed) == tag
        && T.end[n - 1].load(Ordering::Relaxed) == start
        && what_of(n - 1) == what
    {
        T.end[n - 1].store(end, Ordering::Relaxed);
        return;
    }
    if n == N {
        if T.full_said.swap(1, Ordering::Relaxed) == 0 {
            say(format_args!("[gex] memory map: table full at {N} regions; later ones are not checked"));
        }
        return;
    }
    T.start[n].store(start, Ordering::Relaxed);
    T.end[n].store(end, Ordering::Relaxed);
    T.tag[n].store(tag, Ordering::Relaxed);
    T.what[n].store((what.as_ptr() as u64) << 32 | what.len() as u64, Ordering::Relaxed);
    T.len.store(n as u32 + 1, Ordering::Relaxed);
}

fn print_locked(n: usize) {
    say(format_args!("[gex] memory map, {n} regions:"));
    for i in 0..n {
        let t = T.tag[i].load(Ordering::Relaxed);
        say(format_args!(
            "[gex]   {:#010x}..{:#010x} {:<16} {:>3} {}",
            T.start[i].load(Ordering::Relaxed),
            T.end[i].load(Ordering::Relaxed),
            Kind::from(t >> 32).name(),
            t & 0xffff_ffff,
            what_of(i)
        ));
    }
}

/// The whole map, on the console.
pub fn print() {
    let _h = Held::take();
    print_locked(T.len.load(Ordering::Relaxed) as usize);
}

/// Who owns `addr`, as "kind owner what start..end" (the last registered
/// region containing it), or "nobody".
pub fn owner_of(addr: u64) -> String {
    let _h = Held::take();
    let n = T.len.load(Ordering::Relaxed) as usize;
    for i in (0..n).rev() {
        let (s, e) = (T.start[i].load(Ordering::Relaxed), T.end[i].load(Ordering::Relaxed));
        if s <= addr && addr < e {
            let t = T.tag[i].load(Ordering::Relaxed);
            return format!("{} {} ({}) {s:#x}..{e:#x}", Kind::from(t >> 32).name(), t & 0xffff_ffff, what_of(i));
        }
    }
    "nobody".into()
}

/// The parts of `[start, end)` that only the guest owns (no shim heap,
/// stack, thread-local block, foreign growth or fixed runtime region overlaps
/// them), in order. `None` when the table has overflowed and so cannot say.
pub fn guest_only(start: u64, end: u64) -> Option<Vec<(u64, u64)>> {
    let _h = Held::take();
    if T.full_said.load(Ordering::Relaxed) != 0 {
        return None;
    }
    let n = T.len.load(Ordering::Relaxed) as usize;
    let mut out = vec![(start, end)];
    for i in 0..n {
        if Kind::from(T.tag[i].load(Ordering::Relaxed) >> 32).guests() {
            continue;
        }
        let (s, e) = (T.start[i].load(Ordering::Relaxed), T.end[i].load(Ordering::Relaxed));
        let mut next = Vec::with_capacity(out.len() + 1);
        for (a, b) in out {
            if e <= a || s >= b {
                next.push((a, b));
                continue;
            }
            if a < s {
                next.push((a, s));
            }
            if e < b {
                next.push((e, b));
            }
        }
        out = next;
    }
    Some(out)
}

static FIXED: AtomicU32 = AtomicU32::new(0);

/// The fixed layout, once per process (every Worker calls it).
pub fn claim_fixed(heap_base: u64) {
    if FIXED.swap(1, Ordering::AcqRel) != 0 {
        return;
    }
    claim(Kind::Reserved, 0, 0, gex_abi::RESERVED_END, "page zero, contexts, runtime words");
    claim(Kind::Reserved, 0, gex_abi::DISPATCH_DIR, gex_abi::DISPATCH_DIR_END, "dispatch directory");
    if gex_abi::mem64() {
        claim(Kind::Reserved, 0, gex_abi::DISPATCH_DIR64, gex_abi::HIGH_TABLES_END, "M64 dispatch directory, patched pages");
    }
    claim(Kind::Guest, 0, gex_abi::DISPATCH_DIR_END, 0x0580_0000, "low layout: image, interpreter, break, stack");
    claim(Kind::BootStacks, 0, 0x0580_0000, 0x0600_0000, "gex_worker_init's bootstrap slices");
    claim(Kind::ShimData, 0, 0x0600_0000, heap_base, "shim data and bss");
    claim(Kind::Guest, 0, gex_abi::PLACE_BOTTOM, gex_abi::PLACE_TOP, "placement window (shared objects)");
}

/// The first `at' >= at` where `[at', at'+len)` overlaps no foreign region
/// and no island.
pub fn skip_foreign(at: u64, len: u64) -> u64 {
    let _h = Held::take();
    skip_locked(at, len)
}

/// Register `[start, end)` as an island, unless the arena's `cursor` has
/// passed `start` (then the span below it may be the heap's, and the caller
/// moves the cursor instead). Under the table's lock, as [`take_span`] is:
/// a span is never handed out over an island registered while it was
/// being chosen.
pub fn claim_island(start: u64, end: u64, cursor: &AtomicU64, what: &'static str) -> bool {
    let _h = Held::take();
    // A full table checks nothing, and an island it does not hold is one
    // the arena would not step over.
    if cursor.load(Ordering::Acquire) > start || T.len.load(Ordering::Relaxed) as usize >= N {
        return false;
    }
    claim_locked(Kind::Island, 0, start, end, what);
    true
}

/// Whether `[start, end)` lies inside one island: the guest's already, a
/// fixed mapping there needs nothing moved.
pub fn within_island(start: u64, end: u64) -> bool {
    let _h = Held::take();
    let n = T.len.load(Ordering::Relaxed) as usize;
    (0..n).any(|i| {
        Kind::from(T.tag[i].load(Ordering::Relaxed) >> 32) == Kind::Island
            && T.start[i].load(Ordering::Relaxed) <= start
            && end <= T.end[i].load(Ordering::Relaxed)
    })
}

/// Move the arena's `cursor` from `cur` to `at + len`, if `[at, at + len)`
/// overlaps no foreign region or island and nobody moved it meanwhile.
pub fn take_span(cursor: &AtomicU64, cur: u64, at: u64, len: u64) -> bool {
    let _h = Held::take();
    skip_locked(at, len) == at
        && cursor.compare_exchange(cur, at + len, Ordering::AcqRel, Ordering::Acquire).is_ok()
}

fn skip_locked(mut at: u64, len: u64) -> u64 {
    let n = T.len.load(Ordering::Relaxed) as usize;
    loop {
        let mut moved = false;
        for i in 0..n {
            if !matches!(Kind::from(T.tag[i].load(Ordering::Relaxed) >> 32), Kind::Foreign | Kind::Island) {
                continue;
            }
            let (s, e) = (T.start[i].load(Ordering::Relaxed), T.end[i].load(Ordering::Relaxed));
            if s < at + len && at < e {
                at = e;
                moved = true;
            }
        }
        if !moved {
            return at;
        }
    }
}
