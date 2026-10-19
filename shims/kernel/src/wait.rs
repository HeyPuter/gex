//! Wait slots and wait queues (docs/kernel-locking.md §5.2 step 2).
//!
//! A *slot* is one waiting thread: a word here, in the memory every Worker
//! shares, that its Worker parks on (`k_ws_park`) and a waker bumps and
//! notifies. Each Worker runs one guest thread, so a Worker owns one slot for
//! as long as it lives.
//!
//! A *queue* is one waitable object's list of waiters: a bitmap of slots,
//! plus the queues chained to it (an epoll set watching the object). A waiter
//! sets its bit on every object it waits for, checks readiness, parks on its
//! slot, and clears the bits when the wait is over; whoever changes the
//! object's readiness notifies its queue, which wakes the slots whose bits
//! are set and nothing else.
//!
//! Lock-free on both sides: registration is a `fetch_or`, a notify reads the
//! bitmap and bumps the slots in it. A waiter samples its slot's value
//! *before* it registers and checks, and parks only while the slot still
//! reads that value -- so a notify between its check and its park is not
//! lost. (wasm's atomics are sequentially consistent, which is what makes the
//! register-then-check against change-then-notify order sound.)
//!
//! The page reaches the same structures from JavaScript (`wl-bridge.mjs`
//! wakes a process's compositor connection this way), so the layout is
//! fixed and simple: see `Queue` and `k_wq_dir`.

use std::sync::atomic::{AtomicI32, AtomicPtr, AtomicU32, Ordering::SeqCst};

/// How many slots: more than the Workers that can exist at once
/// (`STACK_COUNT`), each of which owns at most one, and the waiters that are
/// not Workers' guest threads (`k_ws_alloc`).
pub const SLOTS: usize = 2048;
const WORDS: usize = SLOTS / 32;
/// Queues chained to one queue, kept inline (JavaScript walks them too).
/// More spill into a map only Rust reads.
const CHAINS: usize = 4;

/// One queue: 320 bytes. Fixed layout, read by JavaScript: the slot bitmap
/// (`WORDS` words, 64), then the inline chains (4), then the spill count.
#[repr(C, align(64))]
pub struct Queue {
    bits: [AtomicU32; WORDS],
    chains: [AtomicU32; CHAINS],
    spilled: AtomicU32,
    /// 1 while the queue is allocated, for a diagnostic.
    live: AtomicU32,
    _pad: [u32; 10],
}

const QUEUE_BYTES: usize = 320;
const _: () = assert!(std::mem::size_of::<Queue>() == QUEUE_BYTES);

/// Queues are allocated in chunks, never freed (a freed queue's number is
/// reused), so a queue's address is stable and JavaScript can cache it.
const CHUNK: usize = 1024;
const CHUNKS: usize = 256;

static DIR: [AtomicPtr<Queue>; CHUNKS] = [const { AtomicPtr::new(std::ptr::null_mut()) }; CHUNKS];

/// The slot words.
#[repr(C, align(64))]
struct Slots([AtomicI32; SLOTS]);
static SLOT_WORDS: Slots = Slots([const { AtomicI32::new(0) }; SLOTS]);
/// Which slots are in use.
static SLOT_USED: [AtomicU32; WORDS] = [const { AtomicU32::new(0) }; WORDS];
/// How many slot notifies there have been, ever: the single-thread build
/// (where nothing can block in `memory.atomic.wait32`) compares it after a
/// call to learn whether a parked fiber may have been woken.
static NOTIFIES: AtomicU32 = AtomicU32::new(0);

/// A tiny spin lock, for the queue allocator and the spill map: the page's
/// own thread notifies (a keystroke) and must never block in a wait.
struct Spin(AtomicU32);
impl Spin {
    fn lock(&self) -> SpinGuard<'_> {
        while self.0.compare_exchange_weak(0, 1, SeqCst, SeqCst).is_err() {
            std::hint::spin_loop();
        }
        SpinGuard(self)
    }
}
struct SpinGuard<'a>(&'a Spin);
impl Drop for SpinGuard<'_> {
    fn drop(&mut self) {
        self.0 .0.store(0, SeqCst);
    }
}

static ALLOC: Spin = Spin(AtomicU32::new(0));
/// Under `ALLOC`: the next never-used queue number, and the freed ones.
static mut NEXT: u32 = 1;
static mut FREE: Vec<u32> = Vec::new();
static SPILL_LOCK: Spin = Spin(AtomicU32::new(0));
/// Under `SPILL_LOCK`: chains past the inline ones, by queue.
static mut SPILL: Option<std::collections::BTreeMap<u32, Vec<u32>>> = None;

fn queue(id: u32) -> Option<&'static Queue> {
    let (c, i) = (id as usize / CHUNK, id as usize % CHUNK);
    if id == 0 || c >= CHUNKS {
        return None;
    }
    let p = DIR[c].load(SeqCst);
    if p.is_null() {
        return None;
    }
    // SAFETY: a published chunk is CHUNK queues, never freed.
    Some(unsafe { &*p.add(i) })
}

pub fn new_queue() -> u32 {
    let _g = ALLOC.lock();
    // SAFETY: `NEXT` and `FREE` are only touched under `ALLOC`.
    let id = unsafe {
        let free = &mut *std::ptr::addr_of_mut!(FREE);
        match free.pop() {
            Some(id) => id,
            None => {
                let id = NEXT;
                let c = id as usize / CHUNK;
                if c >= CHUNKS {
                    return 0;
                }
                if DIR[c].load(SeqCst).is_null() {
                    let chunk: Box<[Queue]> = (0..CHUNK)
                        .map(|_| Queue {
                            bits: [const { AtomicU32::new(0) }; WORDS],
                            chains: [const { AtomicU32::new(0) }; CHAINS],
                            spilled: AtomicU32::new(0),
                            live: AtomicU32::new(0),
                            _pad: [0; 10],
                        })
                        .collect();
                    DIR[c].store(Box::leak(chunk).as_mut_ptr(), SeqCst);
                }
                NEXT += 1;
                id
            }
        }
    };
    if let Some(q) = queue(id) {
        q.live.store(1, SeqCst);
    }
    id
}

pub fn free_queue(id: u32) {
    let Some(q) = queue(id) else { return };
    // Whoever waits on a vanishing object must look again: its last close
    // is a hang-up or an end of file.
    notify(id);
    for w in &q.bits {
        w.store(0, SeqCst);
    }
    for c in &q.chains {
        c.store(0, SeqCst);
    }
    if q.spilled.swap(0, SeqCst) != 0 {
        let _g = SPILL_LOCK.lock();
        // SAFETY: under `SPILL_LOCK`.
        if let Some(m) = unsafe { &mut *std::ptr::addr_of_mut!(SPILL) } {
            m.remove(&id);
        }
    }
    q.live.store(0, SeqCst);
    let _g = ALLOC.lock();
    // SAFETY: under `ALLOC`.
    unsafe { (*std::ptr::addr_of_mut!(FREE)).push(id) };
}

fn wake_slot(s: usize) {
    let w = &SLOT_WORDS.0[s];
    w.fetch_add(1, SeqCst);
    NOTIFIES.fetch_add(1, SeqCst);
    // SAFETY: the address of a static in this module's shared memory.
    unsafe {
        core::arch::wasm32::memory_atomic_notify(w as *const AtomicI32 as *mut i32, u32::MAX);
    }
}

fn notify_depth(id: u32, depth: u32) {
    let Some(q) = queue(id) else { return };
    for (wi, w) in q.bits.iter().enumerate() {
        let mut bits = w.load(SeqCst);
        while bits != 0 {
            let b = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            wake_slot(wi * 32 + b);
        }
    }
    // An epoll set inside an epoll set inside...: Linux bounds the nesting
    // too (EPOLL_MAX_NESTS, 4).
    if depth >= 4 {
        return;
    }
    for c in &q.chains {
        let to = c.load(SeqCst);
        if to != 0 {
            notify_depth(to, depth + 1);
        }
    }
    if q.spilled.load(SeqCst) != 0 {
        let more = {
            let _g = SPILL_LOCK.lock();
            // SAFETY: under `SPILL_LOCK`.
            unsafe { &*std::ptr::addr_of!(SPILL) }
                .as_ref()
                .and_then(|m| m.get(&id).cloned())
                .unwrap_or_default()
        };
        for to in more {
            notify_depth(to, depth + 1);
        }
    }
}

pub fn notify(id: u32) {
    notify_depth(id, 0);
}

fn chain(from: u32, to: u32) {
    let Some(q) = queue(from) else { return };
    for c in &q.chains {
        if c.compare_exchange(0, to, SeqCst, SeqCst).is_ok() {
            return;
        }
    }
    let _g = SPILL_LOCK.lock();
    // SAFETY: under `SPILL_LOCK`.
    let m = unsafe { &mut *std::ptr::addr_of_mut!(SPILL) }.get_or_insert_with(Default::default);
    m.entry(from).or_default().push(to);
    q.spilled.store(1, SeqCst);
}

fn unchain(from: u32, to: u32) {
    let Some(q) = queue(from) else { return };
    for c in &q.chains {
        if c.compare_exchange(to, 0, SeqCst, SeqCst).is_ok() {
            return;
        }
    }
    if q.spilled.load(SeqCst) != 0 {
        let _g = SPILL_LOCK.lock();
        // SAFETY: under `SPILL_LOCK`.
        if let Some(m) = unsafe { &mut *std::ptr::addr_of_mut!(SPILL) } {
            if let Some(v) = m.get_mut(&from) {
                if let Some(i) = v.iter().position(|&x| x == to) {
                    v.swap_remove(i);
                }
                if v.is_empty() {
                    m.remove(&from);
                    q.spilled.store(0, SeqCst);
                }
            }
        }
    }
}

/// Install the queues as `gex_kernel::wait`'s, for the objects this module
/// keeps (pipes, sockets, ptys, the process table).
pub fn install() {
    gex_kernel::wait::set_hooks(gex_kernel::wait::Hooks {
        new: new_queue,
        free: free_queue,
        notify,
        chain,
        unchain,
    });
}

// -- exports -----------------------------------------------------------------

/// A new queue, for an object a process keeps (an eventfd, an epoll set).
#[no_mangle]
pub extern "C" fn k_wq_new() -> u32 {
    new_queue()
}

#[no_mangle]
pub extern "C" fn k_wq_free(id: u32) {
    free_queue(id);
}

#[no_mangle]
pub extern "C" fn k_wq_notify(id: u32) {
    notify(id);
}

#[no_mangle]
pub extern "C" fn k_wq_chain(from: u32, to: u32) {
    chain(from, to);
}

#[no_mangle]
pub extern "C" fn k_wq_unchain(from: u32, to: u32) {
    unchain(from, to);
}

/// Register slot `slot` on queue `id`.
#[no_mangle]
pub extern "C" fn k_wq_add(id: u32, slot: u32) {
    if let Some(q) = queue(id) {
        let s = slot as usize % SLOTS;
        q.bits[s / 32].fetch_or(1 << (s % 32), SeqCst);
    }
}

/// Undo [`k_wq_add`].
#[no_mangle]
pub extern "C" fn k_wq_del(id: u32, slot: u32) {
    if let Some(q) = queue(id) {
        let s = slot as usize % SLOTS;
        q.bits[s / 32].fetch_and(!(1 << (s % 32)), SeqCst);
    }
}

/// Where queue `id` is in this module's memory, or 0: JavaScript notifies a
/// queue by walking it (`wl-bridge.mjs`, `notifyQueue`).
#[no_mangle]
pub extern "C" fn k_wq_addr(id: u32) -> u32 {
    queue(id).map_or(0, |q| q as *const Queue as u32)
}

/// Where the chunk directory is: `CHUNKS` addresses, each of `CHUNK`
/// queues of `QUEUE_BYTES`, for JavaScript to find a chained queue by number.
/// (`wl-bridge.mjs` `notifyQueue` mirrors these constants.)
#[no_mangle]
pub extern "C" fn k_wq_dir() -> u32 {
    DIR.as_ptr() as u32
}

/// The calling Worker's own slot: its kernel stack's number, plus one, or
/// 0 on the stack wasm-ld linked in (the first Worker's). A Worker's stack
/// slice is its for as long as it lives, so the slot is too, and needs no
/// freeing; a slice handed to a new Worker hands it the slot, and anything
/// still registered there is at worst a spurious wake, which re-checks.
#[no_mangle]
pub extern "C" fn k_ws_self() -> i32 {
    let local = 0u8;
    let at = core::hint::black_box(&local) as *const u8 as u32;
    let base = crate::k_stack_area();
    let end = base + crate::STACK_BYTES * crate::STACK_COUNT;
    if at >= base && at < end {
        ((at - base) / crate::STACK_BYTES + 1) as i32
    } else {
        0
    }
}

/// The first slot `k_ws_alloc` hands out: those below are the Workers'
/// (`k_ws_self`).
const FIRST_ALLOC: usize = crate::STACK_COUNT as usize + 1;
const _: () = assert!(FIRST_ALLOC < SLOTS);

/// Take a free slot for a waiter that is not a Worker's guest thread (the
/// page's X server, `hostx.js`), or -1.
#[no_mangle]
pub extern "C" fn k_ws_alloc() -> i32 {
    for (wi, w) in SLOT_USED.iter().enumerate() {
        let mut cur = w.load(SeqCst);
        // The Workers' own slots are never handed out.
        let mut reserved = 0u32;
        for b in 0..32 {
            if wi * 32 + b < FIRST_ALLOC {
                reserved |= 1 << b;
            }
        }
        while cur | reserved != u32::MAX {
            let b = (!(cur | reserved)).trailing_zeros();
            match w.compare_exchange(cur, cur | (1 << b), SeqCst, SeqCst) {
                Ok(_) => return (wi * 32 + b as usize) as i32,
                Err(now) => cur = now,
            }
        }
    }
    -1
}

/// Give a slot back (its Worker is ending).
#[no_mangle]
pub extern "C" fn k_ws_free(slot: i32) {
    if slot >= 0 && (slot as usize) < SLOTS {
        let s = slot as usize;
        SLOT_USED[s / 32].fetch_and(!(1 << (s % 32)), SeqCst);
    }
}

/// What slot `slot` reads now: sampled before a waiter registers and checks.
#[no_mangle]
pub extern "C" fn k_ws_seq(slot: i32) -> i32 {
    SLOT_WORDS.0[slot as usize % SLOTS].load(SeqCst)
}

/// Where slot `slot`'s word is: a Worker that must keep its event loop
/// turning (`gex.mjs` `#netWait`) waits on it with `Atomics.waitAsync`.
#[no_mangle]
pub extern "C" fn k_ws_addr(slot: i32) -> u32 {
    &SLOT_WORDS.0[slot as usize % SLOTS] as *const AtomicI32 as u32
}

/// Sleep until slot `slot` leaves `seq`, or `ns` pass: 0 woken, 1 it had
/// already moved, 2 timed out. A negative `ns` waits for ever.
#[no_mangle]
pub extern "C" fn k_ws_park(slot: i32, seq: i32, ns: i64) -> i32 {
    let w = &SLOT_WORDS.0[slot as usize % SLOTS];
    // SAFETY: the address of a static in this module's shared memory, which
    // is what the instruction requires; only a Worker calls this.
    unsafe { core::arch::wasm32::memory_atomic_wait32(w as *const AtomicI32 as *mut i32, seq, ns) }
}

/// Wake slot `slot` directly: a signal to the thread that owns it.
#[no_mangle]
pub extern "C" fn k_ws_wake(slot: i32) {
    if slot >= 0 {
        wake_slot(slot as usize % SLOTS);
    }
}

/// How many slot wakes there have been (see `NOTIFIES`).
#[no_mangle]
pub extern "C" fn k_ws_notifies() -> u32 {
    NOTIFIES.load(SeqCst)
}

/// How many queues are allocated now, for a census.
#[no_mangle]
pub extern "C" fn k_wq_live() -> u32 {
    let _g = ALLOC.lock();
    // SAFETY: under `ALLOC`.
    unsafe { NEXT - 1 - (*std::ptr::addr_of!(FREE)).len() as u32 }
}
