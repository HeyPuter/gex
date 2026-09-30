//! Dispatcher objects and waits (docs/nt-layer.md §3.3).
//!
//! Every thread sleeps on its own wait word, a `u32` in guest memory, with the
//! process's futex table, so the same code blocks natively (a host thread) and
//! in the browser (`Atomics.wait` in a Worker). Object state lives in one
//! [`SyncCore`] per process under one lock, which is what makes wait-all
//! atomic. A waiter registers on every object it waits for; signalling an
//! object sets its waiters' words and wakes them, and each re-checks.

use super::object::{self, Obj};
use super::{st, Call, Status, Thread, R};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncKind {
    Event,
    Mutant,
    Semaphore,
    KeyedEvent,
}

impl SyncKind {
    pub fn type_name(self) -> &'static str {
        match self {
            SyncKind::Event => "Event",
            SyncKind::Mutant => "Mutant",
            SyncKind::Semaphore => "Semaphore",
            SyncKind::KeyedEvent => "KeyedEvent",
        }
    }
}

#[derive(Debug)]
pub struct SyncObj {
    pub id: u64,
    pub kind: SyncKind,
}

#[derive(Clone, Copy, Debug)]
pub enum SState {
    Event { set: bool, manual: bool },
    Mutant { owner: u32, count: u32, abandoned: bool },
    Semaphore { count: i32, max: i32 },
    /// A thread or process: signalled once it has ended.
    Done(bool),
    /// Never signalled (the current process while it runs, a keyed event).
    Never,
    /// Always signalled (files, which complete I/O synchronously).
    Always,
}

#[derive(Default, Debug)]
pub struct SyncCore {
    pub states: HashMap<u64, SState>,
    waiters: HashMap<u64, Vec<Arc<Thread>>>,
}

impl SyncCore {
    fn signalled(&self, id: u64, me: u32) -> bool {
        match self.states.get(&id) {
            Some(SState::Event { set, .. }) => *set,
            Some(SState::Mutant { owner, .. }) => *owner == 0 || *owner == me,
            Some(SState::Semaphore { count, .. }) => *count > 0,
            Some(SState::Done(d)) => *d,
            Some(SState::Always) => true,
            Some(SState::Never) | None => false,
        }
    }

    /// Take what a satisfied wait takes. True if a mutant was abandoned.
    fn consume(&mut self, id: u64, me: u32) -> bool {
        match self.states.get_mut(&id) {
            Some(SState::Event { set, manual }) if !*manual => {
                *set = false;
                false
            }
            Some(SState::Mutant { owner, count, abandoned }) => {
                *owner = me;
                *count += 1;
                std::mem::take(abandoned)
            }
            Some(SState::Semaphore { count, .. }) => {
                *count -= 1;
                false
            }
            _ => false,
        }
    }

    fn register(&mut self, id: u64, t: &Arc<Thread>) {
        let w = self.waiters.entry(id).or_default();
        if !w.iter().any(|x| Arc::ptr_eq(x, t)) {
            w.push(Arc::clone(t));
        }
    }

    fn unregister(&mut self, id: u64, t: &Arc<Thread>) {
        if let Some(w) = self.waiters.get_mut(&id) {
            w.retain(|x| !Arc::ptr_eq(x, t));
            if w.is_empty() {
                self.waiters.remove(&id);
            }
        }
    }

    /// The threads to wake after `id` became signalled.
    fn to_wake(&self, id: u64) -> Vec<Arc<Thread>> {
        self.waiters.get(&id).cloned().unwrap_or_default()
    }
}

/// Set a thread's wait word and wake it. Guest memory is written through
/// `mem` (any view of the shared memory).
pub fn wake_thread(c: &mut Call<'_>, t: &Thread) {
    let _ = c.g.w32(t.word, 1);
    c.p.futexes.wake(t.word, u32::MAX);
}

fn wake_all(c: &mut Call<'_>, ts: Vec<Arc<Thread>>) {
    for t in ts {
        wake_thread(c, &t);
    }
}

/// Mark `id` signalled-ish after a state change and wake its waiters.
pub fn changed(c: &mut Call<'_>, id: u64) {
    let ts = c.p.sync.lock().unwrap().to_wake(id);
    wake_all(c, ts);
}

/// An NT timeout (`LARGE_INTEGER *`) as a monotonic deadline in ns.
/// `None` is "forever".
pub fn deadline(c: &Call<'_>, p: u64) -> R<Option<u64>> {
    if p == 0 {
        return Ok(None);
    }
    let v = c.g.u64(p)? as i64;
    let now = (c.p.clocks.mono_ns)();
    Ok(Some(if v <= 0 {
        now.saturating_add((v.unsigned_abs()).saturating_mul(100))
    } else {
        // Absolute, in system time.
        let wall = c.p.clocks.filetime() as i64;
        now.saturating_add(((v - wall).max(0) as u64).saturating_mul(100))
    }))
}

/// The sync-core id and a fresh state for a waitable handle.
fn wait_id(c: &Call<'_>, h: u32) -> R<u64> {
    match object::resolve(c, h)? {
        Obj::Sync(s) => Ok(s.id),
        Obj::Thread(t) => Ok(t.sync_id),
        Obj::Process => Ok(c.p.sync_id),
        Obj::File(_) => Ok(ALWAYS_ID),
        _ => Err(Status(st::OBJECT_TYPE_MISMATCH)),
    }
}

/// The id of a permanently signalled pseudo-object (files).
const ALWAYS_ID: u64 = 0;

/// Wait for `ids`. Returns `WAIT_0 + i`, `ABANDONED + i`, `TIMEOUT`,
/// `USER_APC` or `ALERTED`.
pub fn wait(c: &mut Call<'_>, ids: &[u64], all: bool, alertable: bool, deadline: Option<u64>) -> R {
    let me = c.t.tid;
    let t = Arc::clone(&c.t);
    {
        let mut core = c.p.sync.lock().unwrap();
        core.states.entry(ALWAYS_ID).or_insert(SState::Always);
        core.states.entry(c.p.sync_id).or_insert(SState::Never);
    }
    loop {
        {
            let mut core = c.p.sync.lock().unwrap();
            if all {
                if ids.iter().all(|&id| core.signalled(id, me)) {
                    let mut abandoned = false;
                    for &id in ids {
                        abandoned |= core.consume(id, me);
                        core.unregister(id, &t);
                    }
                    return Ok(if abandoned { st::ABANDONED } else { st::WAIT_0 });
                }
            } else if let Some(i) = ids.iter().position(|&id| core.signalled(id, me)) {
                let abandoned = core.consume(ids[i], me);
                for &id in ids {
                    core.unregister(id, &t);
                }
                return Ok(if abandoned { st::ABANDONED } else { st::WAIT_0 } + i as u32);
            }
            if alertable && !t.apcs.lock().unwrap().is_empty() {
                for &id in ids {
                    core.unregister(id, &t);
                }
                return Ok(st::USER_APC);
            }
            for &id in ids {
                core.register(id, &t);
            }
            // Cleared under the lock: a signal after this sets it again.
            let _ = c.g.w32(t.word, 0);
        }
        if c.p.terminating.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(Status(st::THREAD_IS_TERMINATING));
        }
        let timeout = match deadline {
            None => None,
            Some(d) => {
                let now = (c.p.clocks.mono_ns)();
                if now >= d {
                    let mut core = c.p.sync.lock().unwrap();
                    for &id in ids {
                        core.unregister(id, &t);
                    }
                    return Ok(st::TIMEOUT);
                }
                Some(Duration::from_nanos(d - now))
            }
        };
        sleep_on_word(c, &t, timeout);
    }
}

/// Sleep on `t`'s wait word while it reads 0, until woken or `timeout`.
fn sleep_on_word(c: &Call<'_>, t: &Thread, timeout: Option<Duration>) {
    let g = &c.g;
    let word = t.word;
    // A bounded slice even for "forever", so a process that is ending is
    // noticed by threads parked here.
    let slice = Duration::from_millis(200);
    let d = timeout.map_or(slice, |t| t.min(slice));
    let _ = c.p.futexes.wait(word, 0, Some(d), &|| g.u32(word).ok());
}

fn wait_on_handles(c: &mut Call<'_>, hs: &[u32], all: bool, alertable: bool, timeout_p: u64) -> R {
    let mut ids = Vec::with_capacity(hs.len());
    for &h in hs {
        ids.push(wait_id(c, h)?);
    }
    let d = deadline(c, timeout_p)?;
    let r = wait(c, &ids, all, alertable, d)?;
    if r == st::USER_APC {
        super::thread::deliver_apc(c)?;
    }
    Ok(r)
}

/// `NtWaitForSingleObject(handle, alertable, timeout)`.
pub fn nt_wait_for_single_object(c: &mut Call<'_>) -> R {
    let (h, alertable, tp) = (c.a32(0), c.a32(1) & 0xff != 0, c.a(2));
    wait_on_handles(c, &[h], false, alertable, tp)
}

/// `NtWaitForMultipleObjects(count, handles, type, alertable, timeout)`.
/// Type 0 is WaitAll, 1 is WaitAny.
pub fn nt_wait_for_multiple_objects(c: &mut Call<'_>) -> R {
    let (n, hp, ty, alertable, tp) = (c.a32(0), c.a(1), c.a32(2), c.a32(3) & 0xff != 0, c.a(4));
    if n == 0 || n > 64 {
        return Err(Status(st::INVALID_PARAMETER_1));
    }
    let ps = c.g.psize();
    let mut hs = Vec::with_capacity(n as usize);
    for i in 0..n as u64 {
        hs.push(c.g.ptr(hp + i * ps)? as u32);
    }
    wait_on_handles(c, &hs, ty == 0, alertable, tp)
}

/// `NtSignalAndWaitForSingleObject(signal, wait, alertable, timeout)`.
pub fn nt_signal_and_wait_for_single_object(c: &mut Call<'_>) -> R {
    let (sh, wh, alertable, tp) = (c.a32(0), c.a32(1), c.a32(2) & 0xff != 0, c.a(3));
    match object::resolve(c, sh)? {
        Obj::Sync(s) => match s.kind {
            SyncKind::Event => {
                set_event(c, &s, true)?;
            }
            SyncKind::Semaphore => {
                release_semaphore(c, &s, 1)?;
            }
            SyncKind::Mutant => {
                release_mutant(c, &s)?;
            }
            SyncKind::KeyedEvent => return Err(Status(st::OBJECT_TYPE_MISMATCH)),
        },
        _ => return Err(Status(st::OBJECT_TYPE_MISMATCH)),
    }
    wait_on_handles(c, &[wh], false, alertable, tp)
}

/// `NtWaitForAlertByThreadId(address, timeout)`: Wine's futex wait.
pub fn nt_wait_for_alert_by_thread_id(c: &mut Call<'_>) -> R {
    let tp = c.a(1);
    let d = deadline(c, tp)?;
    let t = Arc::clone(&c.t);
    use std::sync::atomic::Ordering;
    loop {
        if t.alerted.swap(false, Ordering::AcqRel) {
            return Ok(st::ALERTED);
        }
        let _ = c.g.w32(t.word, 0);
        if t.alerted.swap(false, Ordering::AcqRel) {
            return Ok(st::ALERTED);
        }
        let timeout = match d {
            None => None,
            Some(d) => {
                let now = (c.p.clocks.mono_ns)();
                if now >= d {
                    return Ok(st::TIMEOUT);
                }
                Some(Duration::from_nanos(d - now))
            }
        };
        sleep_on_word(c, &t, timeout);
        if c.p.terminating.load(Ordering::Relaxed) {
            return Err(Status(st::THREAD_IS_TERMINATING));
        }
    }
}

/// `NtAlertThreadByThreadId(tid)`.
pub fn nt_alert_thread_by_thread_id(c: &mut Call<'_>) -> R {
    let tid = c.a32(0);
    let t = c.p.thread_by_tid(tid).ok_or(Status(st::INVALID_CID))?;
    t.alerted.store(true, std::sync::atomic::Ordering::Release);
    wake_thread(c, &t);
    Ok(st::SUCCESS)
}

/// `NtDelayExecution(alertable, timeout)`.
pub fn nt_delay_execution(c: &mut Call<'_>) -> R {
    let (alertable, tp) = (c.a32(0) & 0xff != 0, c.a(1));
    let d = deadline(c, tp)?;
    let now = (c.p.clocks.mono_ns)();
    if d.is_some_and(|d| d <= now) {
        std::thread::yield_now();
        return Ok(st::SUCCESS);
    }
    let t = Arc::clone(&c.t);
    loop {
        let now = (c.p.clocks.mono_ns)();
        let left = match d {
            Some(d) if now >= d => return Ok(st::SUCCESS),
            Some(d) => Some(Duration::from_nanos(d - now)),
            None => None,
        };
        if alertable && !t.apcs.lock().unwrap().is_empty() {
            super::thread::deliver_apc(c)?;
            return Ok(st::USER_APC);
        }
        if c.p.terminating.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(st::SUCCESS);
        }
        let _ = c.g.w32(t.word, 0);
        sleep_on_word(c, &t, left);
    }
}

/// `NtYieldExecution()`.
pub fn nt_yield_execution(_c: &mut Call<'_>) -> R {
    std::thread::yield_now();
    Ok(st::SUCCESS)
}

// ---------------------------------------------------------------------------
// Creating and signalling
// ---------------------------------------------------------------------------

/// Create (or, with `OBJ_OPENIF` and a name, open) a sync object.
fn create(c: &mut Call<'_>, hp: u64, access: u32, attr_p: u64, kind: SyncKind, state: SState) -> R {
    let name = match c.g.obj_attr(attr_p)? {
        Some(a) if !a.name.is_empty() => Some((object::object_name(c, a.root, &a.name)?, a.attrs)),
        _ => None,
    };
    let mut stl = c.p.st.lock().unwrap();
    if let Some((n, attrs)) = &name {
        let key = n.to_ascii_lowercase();
        if let Some(existing) = stl.handles.named.get(&key).cloned() {
            let Obj::Sync(s) = &existing else { return Err(Status(st::OBJECT_TYPE_MISMATCH)) };
            if s.kind != kind {
                return Err(Status(st::OBJECT_TYPE_MISMATCH));
            }
            if attrs & super::obj_flags::OPENIF == 0 {
                return Err(Status(st::OBJECT_NAME_COLLISION));
            }
            let h = stl.handles.insert(existing, access);
            drop(stl);
            c.g.wptr(hp, h as u64)?;
            return Ok(st::OBJECT_NAME_EXISTS);
        }
    }
    let obj = Arc::new(SyncObj { id: c.p.new_sync_id(), kind });
    c.p.sync.lock().unwrap().states.insert(obj.id, state);
    if let Some((n, _)) = &name {
        stl.handles.named.insert(n.to_ascii_lowercase(), Obj::Sync(Arc::clone(&obj)));
    }
    let h = stl.handles.insert(Obj::Sync(obj), access);
    drop(stl);
    c.g.wptr(hp, h as u64)?;
    Ok(st::SUCCESS)
}

fn open(c: &mut Call<'_>, hp: u64, access: u32, attr_p: u64, kind: SyncKind) -> R {
    let a = c.g.obj_attr(attr_p)?.ok_or(Status(st::INVALID_PARAMETER))?;
    let name = object::object_name(c, a.root, &a.name)?.to_ascii_lowercase();
    let mut stl = c.p.st.lock().unwrap();
    match stl.handles.named.get(&name).cloned() {
        Some(o @ Obj::Sync(_)) if matches!(&o, Obj::Sync(s) if s.kind == kind) => {
            let h = stl.handles.insert(o, access);
            drop(stl);
            c.g.wptr(hp, h as u64)?;
            Ok(st::SUCCESS)
        }
        Some(_) => Err(Status(st::OBJECT_TYPE_MISMATCH)),
        None => Err(Status(st::OBJECT_NAME_NOT_FOUND)),
    }
}

fn get_sync(c: &Call<'_>, h: u32, kind: SyncKind) -> R<Arc<SyncObj>> {
    match object::resolve(c, h)? {
        Obj::Sync(s) if s.kind == kind => Ok(s),
        _ => Err(Status(st::OBJECT_TYPE_MISMATCH)),
    }
}

/// Set an event; returns the previous state.
fn set_event(c: &mut Call<'_>, s: &SyncObj, to: bool) -> R<bool> {
    let prev = {
        let mut core = c.p.sync.lock().unwrap();
        match core.states.get_mut(&s.id) {
            Some(SState::Event { set, .. }) => std::mem::replace(set, to),
            _ => return Err(Status(st::OBJECT_TYPE_MISMATCH)),
        }
    };
    if to {
        changed(c, s.id);
    }
    Ok(prev)
}

/// Set the event behind a handle (I/O completion).
pub fn set_event_handle(c: &mut Call<'_>, h: u32) -> R<()> {
    let s = get_sync(c, h, SyncKind::Event)?;
    set_event(c, &s, true).map(|_| ())
}

/// `NtCreateEvent(*handle, access, attr, type, initial)`. Type 0 is a
/// notification (manual-reset) event, 1 a synchronization (auto-reset) one.
pub fn nt_create_event(c: &mut Call<'_>) -> R {
    let (hp, access, attr, ty, init) = (c.a(0), c.a32(1), c.a(2), c.a32(3), c.a32(4) & 0xff != 0);
    if ty > 1 {
        return Err(Status(st::INVALID_PARAMETER));
    }
    create(c, hp, access, attr, SyncKind::Event, SState::Event { set: init, manual: ty == 0 })
}

pub fn nt_open_event(c: &mut Call<'_>) -> R {
    let (hp, access, attr) = (c.a(0), c.a32(1), c.a(2));
    open(c, hp, access, attr, SyncKind::Event)
}

/// `NtSetEvent(handle, *prev)`.
pub fn nt_set_event(c: &mut Call<'_>) -> R {
    let (h, prev_p) = (c.a32(0), c.a(1));
    let s = get_sync(c, h, SyncKind::Event)?;
    let prev = set_event(c, &s, true)?;
    c.g.out32(prev_p, prev as u32)?;
    Ok(st::SUCCESS)
}

/// `NtResetEvent(handle, *prev)`.
pub fn nt_reset_event(c: &mut Call<'_>) -> R {
    let (h, prev_p) = (c.a32(0), c.a(1));
    let s = get_sync(c, h, SyncKind::Event)?;
    let prev = set_event(c, &s, false)?;
    c.g.out32(prev_p, prev as u32)?;
    Ok(st::SUCCESS)
}

/// `NtClearEvent(handle)`.
pub fn nt_clear_event(c: &mut Call<'_>) -> R {
    let s = get_sync(c, c.a32(0), SyncKind::Event)?;
    set_event(c, &s, false)?;
    Ok(st::SUCCESS)
}

/// `NtPulseEvent(handle, *prev)`: release current waiters, leave it reset.
/// Waiters re-check after waking, so a pulse that no waiter observes before
/// the reset is lost -- which Windows documents as possible too.
pub fn nt_pulse_event(c: &mut Call<'_>) -> R {
    Err(c.unimplemented("NtPulseEvent (waiters would miss the pulse)"))
}

/// `NtCreateMutant(*handle, access, attr, owned)`.
pub fn nt_create_mutant(c: &mut Call<'_>) -> R {
    let (hp, access, attr, owned) = (c.a(0), c.a32(1), c.a(2), c.a32(3) & 0xff != 0);
    let me = c.t.tid;
    let state = SState::Mutant { owner: if owned { me } else { 0 }, count: owned as u32, abandoned: false };
    create(c, hp, access, attr, SyncKind::Mutant, state)
}

pub fn nt_open_mutant(c: &mut Call<'_>) -> R {
    let (hp, access, attr) = (c.a(0), c.a32(1), c.a(2));
    open(c, hp, access, attr, SyncKind::Mutant)
}

fn release_mutant(c: &mut Call<'_>, s: &SyncObj) -> R<u32> {
    let me = c.t.tid;
    let (prev, freed) = {
        let mut core = c.p.sync.lock().unwrap();
        match core.states.get_mut(&s.id) {
            Some(SState::Mutant { owner, count, .. }) => {
                if *owner != me {
                    return Err(Status(st::MUTANT_NOT_OWNED));
                }
                let prev = *count;
                *count -= 1;
                if *count == 0 {
                    *owner = 0;
                }
                (prev, *count == 0)
            }
            _ => return Err(Status(st::OBJECT_TYPE_MISMATCH)),
        }
    };
    if freed {
        changed(c, s.id);
    }
    Ok(prev)
}

/// `NtReleaseMutant(handle, *prev_count)`.
pub fn nt_release_mutant(c: &mut Call<'_>) -> R {
    let (h, prev_p) = (c.a32(0), c.a(1));
    let s = get_sync(c, h, SyncKind::Mutant)?;
    let prev = release_mutant(c, &s)?;
    c.g.out32(prev_p, prev)?;
    Ok(st::SUCCESS)
}

/// `NtCreateSemaphore(*handle, access, attr, initial, max)`.
pub fn nt_create_semaphore(c: &mut Call<'_>) -> R {
    let (hp, access, attr, init, max) = (c.a(0), c.a32(1), c.a(2), c.a32(3) as i32, c.a32(4) as i32);
    if max <= 0 || init < 0 || init > max {
        return Err(Status(st::INVALID_PARAMETER));
    }
    create(c, hp, access, attr, SyncKind::Semaphore, SState::Semaphore { count: init, max })
}

pub fn nt_open_semaphore(c: &mut Call<'_>) -> R {
    let (hp, access, attr) = (c.a(0), c.a32(1), c.a(2));
    open(c, hp, access, attr, SyncKind::Semaphore)
}

fn release_semaphore(c: &mut Call<'_>, s: &SyncObj, n: i32) -> R<i32> {
    let prev = {
        let mut core = c.p.sync.lock().unwrap();
        match core.states.get_mut(&s.id) {
            Some(SState::Semaphore { count, max }) => {
                if n <= 0 || *count as i64 + n as i64 > *max as i64 {
                    return Err(Status(st::SEMAPHORE_LIMIT_EXCEEDED));
                }
                let prev = *count;
                *count += n;
                prev
            }
            _ => return Err(Status(st::OBJECT_TYPE_MISMATCH)),
        }
    };
    changed(c, s.id);
    Ok(prev)
}

/// `NtReleaseSemaphore(handle, count, *prev)`.
pub fn nt_release_semaphore(c: &mut Call<'_>) -> R {
    let (h, n, prev_p) = (c.a32(0), c.a32(1) as i32, c.a(2));
    let s = get_sync(c, h, SyncKind::Semaphore)?;
    let prev = release_semaphore(c, &s, n)?;
    c.g.out32(prev_p, prev as u32)?;
    Ok(st::SUCCESS)
}

/// `NtCreateKeyedEvent(*handle, access, attr, flags)`. Creating one is
/// supported; waiting on one (`NtWaitForKeyedEvent`) is not yet.
pub fn nt_create_keyed_event(c: &mut Call<'_>) -> R {
    let (hp, access, attr) = (c.a(0), c.a32(1), c.a(2));
    create(c, hp, access, attr, SyncKind::KeyedEvent, SState::Never)
}

/// A thread or process ended: signal waiters on it.
pub fn signal_done(c: &mut Call<'_>, id: u64) {
    c.p.sync.lock().unwrap().states.insert(id, SState::Done(true));
    changed(c, id);
}
