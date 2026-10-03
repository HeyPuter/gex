//! Message queues: posting, sending (within a thread and between threads),
//! peeking and getting, dispatching, timers, and waiting for input
//! (win32u's message.c and wineserver's queue.c).
//!
//! Each thread's queue is part of [`super::User`]. A thread waits for its
//! queue on its NT wait word (the same futex word its object waits use), so a
//! post or a send from another thread wakes it the same way an event does.

use super::window::{self, wm};
use super::winproc::{self, Pack};
use super::{err, handle_ret, set_last_error, R64};
use crate::nt::{off, st, sync, Call, Status, R};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `QS_*` bits.
pub mod qs {
    pub const KEY: u32 = 0x1;
    pub const MOUSEMOVE: u32 = 0x2;
    pub const MOUSEBUTTON: u32 = 0x4;
    pub const POSTMESSAGE: u32 = 0x8;
    pub const TIMER: u32 = 0x10;
    pub const PAINT: u32 = 0x20;
    pub const SENDMESSAGE: u32 = 0x40;
    pub const HOTKEY: u32 = 0x80;
    pub const ALLPOSTMESSAGE: u32 = 0x100;
    pub const RAWINPUT: u32 = 0x400;
    pub const INPUT: u32 = KEY | MOUSEMOVE | MOUSEBUTTON | RAWINPUT;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Msg {
    pub hwnd: u32,
    pub msg: u32,
    pub wparam: u64,
    pub lparam: u64,
    pub time: u32,
    pub pt: (i32, i32),
}

/// A message sent from another thread, and where its reply goes.
#[derive(Debug)]
pub struct Sent {
    pub msg: Msg,
    pub reply: Arc<Reply>,
    /// Whether the sender used the ANSI entry point.
    pub ansi: bool,
    /// Win32u's own work for the receiving thread (Wine's WM_WINE_*
    /// messages), done there in place of a window procedure call.
    pub internal: Option<Internal>,
}

/// What win32u does on a window's own thread when another thread asks
/// (Wine's `handle_internal_message`).
#[derive(Debug, Clone, Copy)]
pub enum Internal {
    /// WM_WINE_SETWINDOWPOS: `set_window_pos(hwnd, after, x, y, cx, cy, flags)`.
    SetWindowPos { after: u32, x: i32, y: i32, cx: i32, cy: i32, flags: u32 },
}

#[derive(Debug, Default)]
pub struct Reply {
    pub done: AtomicBool,
    pub result: Mutex<u64>,
    /// The sender's Windows tid, to wake.
    pub from: u32,
}

#[derive(Clone, Debug)]
pub struct Timer {
    pub hwnd: u32,
    pub id: u64,
    pub interval_ms: u32,
    /// Next expiry, monotonic nanoseconds.
    pub due: u64,
    /// TIMERPROC, passed in lParam.
    pub proc_: u64,
    pub system: bool,
}

#[derive(Debug, Default)]
pub struct Queue {
    pub posted: VecDeque<Msg>,
    /// Hardware messages (keyboard and mouse), after posted ones.
    pub input: VecDeque<super::input::HwMsg>,
    pub sent: VecDeque<Sent>,
    pub quit: Option<u64>,
    pub timers: Vec<Timer>,
    /// `QS_*` bits that arrived since the queue was last looked at.
    pub changed: u32,
    /// The message being processed, for GetMessageTime/Pos and ReplyMessage.
    pub last_time: u32,
    pub last_pos: (i32, i32),
    pub extra_info: u64,
    /// Replies owed for sent messages being processed now (innermost last).
    pub replying: Vec<Arc<Reply>>,
    next_timer_id: u64,
}

impl Queue {
    /// Drop everything aimed at a destroyed window.
    pub fn forget_window(&mut self, hwnd: u32) {
        self.posted.retain(|m| m.hwnd != hwnd);
        self.input.retain(|m| m.msg.hwnd != hwnd);
        self.timers.retain(|t| t.hwnd != hwnd);
    }
}

/// GetTickCount: milliseconds of the monotonic clock.
pub fn tick_count(c: &Call<'_>) -> u32 {
    ((c.p.clocks.mono_ns)() / 1_000_000) as u32
}

fn now_ns(c: &Call<'_>) -> u64 {
    (c.p.clocks.mono_ns)()
}

/// Wake the thread `tid` (its wait word), if it exists.
pub fn wake_thread(c: &mut Call<'_>, tid: u32) {
    wake(c, tid);
}

fn wake(c: &mut Call<'_>, tid: u32) {
    if let Some(t) = c.p.thread_by_tid(tid) {
        sync::wake_thread(c, &t);
    }
}

/// Tell thread `tid` a window of its needs painting.
pub fn wake_for_paint(c: &mut Call<'_>, tid: u32) {
    c.p.user.lock().unwrap().thread(tid).queue.changed |= qs::PAINT;
    if tid != c.t.tid {
        wake(c, tid);
    }
}

/// Post to the queue of `tid`.
pub fn post_to_thread(c: &mut Call<'_>, tid: u32, m: Msg) {
    {
        let mut u = c.p.user.lock().unwrap();
        let q = &mut u.thread(tid).queue;
        q.posted.push_back(m);
        q.changed |= qs::POSTMESSAGE | qs::ALLPOSTMESSAGE;
    }
    if tid != c.t.tid {
        wake(c, tid);
    }
}

/// Send `msg` to `hwnd`, owned by thread `tid` (not this one): queue it for
/// that thread and wait for the reply, handling messages sent to this thread
/// meanwhile, as Windows does. `None` on timeout.
#[allow(clippy::too_many_arguments)]
pub fn send_to_thread(
    c: &mut Call<'_>,
    tid: u32,
    hwnd: u32,
    msg: u32,
    wparam: u64,
    lparam: u64,
    ansi: bool,
    timeout_ms: Option<u32>,
) -> R<Option<u64>> {
    send_to_thread_with(c, tid, hwnd, msg, wparam, lparam, ansi, timeout_ms, None)
}

/// Have thread `tid`, which owns `hwnd`, do `what` itself and wait for it
/// (Wine sends a WM_WINE_* message: a window's position, style and so on
/// change on the thread that owns it).
pub fn send_internal(c: &mut Call<'_>, tid: u32, hwnd: u32, what: Internal) -> R<u64> {
    const WM_WINE_SETWINDOWPOS: u32 = 0x8000_0001;
    let msg = match what {
        Internal::SetWindowPos { .. } => WM_WINE_SETWINDOWPOS,
    };
    Ok(send_to_thread_with(c, tid, hwnd, msg, 0, 0, false, None, Some(what))?.unwrap_or(0))
}

#[allow(clippy::too_many_arguments)]
fn send_to_thread_with(
    c: &mut Call<'_>,
    tid: u32,
    hwnd: u32,
    msg: u32,
    wparam: u64,
    lparam: u64,
    ansi: bool,
    timeout_ms: Option<u32>,
    internal: Option<Internal>,
) -> R<Option<u64>> {
    let reply = Arc::new(Reply { from: c.t.tid, ..Default::default() });
    let m = Msg { hwnd, msg, wparam, lparam, time: tick_count(c), pt: (0, 0) };
    {
        let mut u = c.p.user.lock().unwrap();
        let q = &mut u.thread(tid).queue;
        q.sent.push_back(Sent { msg: m, reply: Arc::clone(&reply), ansi, internal });
        q.changed |= qs::SENDMESSAGE;
    }
    wake(c, tid);
    let deadline = timeout_ms.map(|ms| now_ns(c) + ms as u64 * 1_000_000);
    let t = Arc::clone(&c.t);
    loop {
        let _ = c.g.w32(t.word, 0);
        if reply.done.load(Ordering::Acquire) {
            return Ok(Some(*reply.result.lock().unwrap()));
        }
        // Messages sent to us while we wait are handled now.
        if process_sent(c)? {
            continue;
        }
        if c.p.terminating.load(Ordering::Relaxed) {
            return Err(Status(st::THREAD_IS_TERMINATING));
        }
        let left = match deadline {
            Some(d) => {
                let now = now_ns(c);
                if now >= d {
                    return Ok(None);
                }
                Some(Duration::from_nanos(d - now))
            }
            None => None,
        };
        sync::sleep_on_word(c, &t, left);
    }
}

/// Handle every message sent to this thread from others. True if any was.
pub fn process_sent(c: &mut Call<'_>) -> R<bool> {
    let tid = c.t.tid;
    let mut any = false;
    loop {
        let next = c.p.user.lock().unwrap().thread(tid).queue.sent.pop_front();
        let Some(s) = next else { return Ok(any) };
        any = true;
        c.p.user.lock().unwrap().thread(tid).queue.replying.push(Arc::clone(&s.reply));
        let r = match s.internal {
            Some(Internal::SetWindowPos { after, x, y, cx, cy, flags }) => {
                window::set_window_pos(c, s.msg.hwnd, after, x, y, cx, cy, flags)? as u64
            }
            None => {
                winproc::call_window_proc(c, s.msg.hwnd, s.msg.msg, s.msg.wparam, s.msg.lparam, &Pack::None, s.ansi, winproc::MAP_SENDMESSAGE)?.0
            }
        };
        c.p.user.lock().unwrap().thread(tid).queue.replying.pop();
        reply(c, &s.reply, r);
    }
}

fn reply(c: &mut Call<'_>, r: &Arc<Reply>, result: u64) {
    if r.done.load(Ordering::Acquire) {
        return;
    }
    *r.result.lock().unwrap() = result;
    r.done.store(true, Ordering::Release);
    let from = r.from;
    wake(c, from);
}

/// `NtUserReplyMessage(result)`: answer the message being processed now.
pub fn nt_user_reply_message(c: &mut Call<'_>) -> R64 {
    let result = c.a(0);
    let tid = c.t.tid;
    let r = c.p.user.lock().unwrap().thread(tid).queue.replying.last().cloned();
    match r {
        Some(r) => {
            reply(c, &r, result);
            Ok(1)
        }
        None => Ok(0),
    }
}

/// Whether `m` passes a GetMessage/PeekMessage filter.
fn matches(u: &super::User, m: &Msg, hwnd: u32, first: u32, last: u32) -> bool {
    let hwnd_ok = match hwnd {
        0 => true,
        // (HWND)-1: thread messages only.
        0xffff_ffff => m.hwnd == 0,
        h => m.hwnd == h || is_descendant(u, h, m.hwnd),
    };
    let range_ok = if first == 0 && last == 0 {
        true
    } else if first <= last {
        m.msg >= first && m.msg <= last
    } else {
        m.msg >= first || m.msg <= last
    };
    hwnd_ok && range_ok
}

fn is_descendant(u: &super::User, parent: u32, mut h: u32) -> bool {
    while let Some(w) = u.windows.get(&h) {
        if w.parent == parent {
            return true;
        }
        if w.style & window::ws::CHILD == 0 {
            return false;
        }
        h = w.parent;
    }
    false
}

/// `PM_*`.
const PM_REMOVE: u32 = 0x1;
const PM_NOYIELD: u32 = 0x2;
const PM_QS_SHIFT: u32 = 16;

/// Look for a message (Wine's `peek_message`): sent messages are handled
/// first, then posted messages, the quit message, and due timers.
pub fn peek(c: &mut Call<'_>, hwnd: u32, first: u32, last: u32, flags: u32) -> R<Option<Msg>> {
    crate::nt::display::pump_input(c);
    process_sent(c)?;
    let tid = c.t.tid;
    let remove = flags & PM_REMOVE != 0;
    let mask = if flags >> PM_QS_SHIFT == 0 { u32::MAX } else { flags >> PM_QS_SHIFT };
    let now = now_ns(c);
    let time = tick_count(c);
    let mut u = c.p.user.lock().unwrap();
    let hwnd = if hwnd == 0 || hwnd == 0xffff_ffff { hwnd } else { window::full(&u, hwnd) };
    let snapshot: Vec<Msg> = u.thread(tid).queue.posted.iter().copied().collect();
    let found = if mask & qs::POSTMESSAGE != 0 { snapshot.iter().position(|m| matches(&u, m, hwnd, first, last)) } else { None };
    let q = &mut u.thread(tid).queue;
    q.changed = 0;
    if let Some(i) = found {
        let m = if remove { q.posted.remove(i).unwrap() } else { q.posted[i] };
        q.last_time = m.time;
        q.last_pos = m.pt;
        return Ok(Some(m));
    }
    if let Some(code) = q.quit {
        if mask & qs::POSTMESSAGE != 0 && (first == 0 && last == 0 || (first..=last).contains(&wm::QUIT)) {
            if remove {
                q.quit = None;
            }
            return Ok(Some(Msg { hwnd: 0, msg: wm::QUIT, wparam: code, lparam: 0, time, pt: q.last_pos }));
        }
    }
    // Hardware input, after posted messages.
    let input_mask = mask & (qs::KEY | qs::MOUSEMOVE | qs::MOUSEBUTTON);
    if input_mask != 0 {
        let pick = {
            let snap: Vec<super::input::HwMsg> = u.thread(tid).queue.input.iter().copied().collect();
            snap.iter().position(|hw| {
                let bit = match hw.msg.msg {
                    0x100..=0x109 => qs::KEY,
                    0x200 => qs::MOUSEMOVE,
                    _ => qs::MOUSEBUTTON,
                };
                input_mask & bit != 0 && matches(&u, &hw.msg, hwnd, first, last)
            })
        };
        if let Some(i) = pick {
            let q = &mut u.thread(tid).queue;
            let hw = if remove { q.input.remove(i).unwrap() } else { q.input[i] };
            q.last_time = hw.msg.time;
            q.last_pos = hw.msg.pt;
            if remove {
                super::input::retrieved(&mut u, tid, &hw);
            }
            return Ok(Some(hw.msg));
        }
    }
    // WM_PAINT: after posted messages, before timers. It stays until the
    // window is validated; removing it only clears an internal paint.
    if mask & qs::PAINT != 0 && (first == 0 && last == 0 || (first..=last).contains(&wm::PAINT)) {
        let filter = if hwnd == 0xffff_ffff { None } else { Some(hwnd) };
        if let Some(h) = filter.and_then(|f| super::paint::window_to_paint(&u, tid, f)) {
            if remove {
                if let Some(w) = u.windows.get_mut(&h) {
                    w.internal_paint = false;
                }
            }
            let q = &mut u.thread(tid).queue;
            q.last_time = time;
            return Ok(Some(Msg { hwnd: h, msg: wm::PAINT, wparam: 0, lparam: 0, time, pt: q.last_pos }));
        }
    }
    let q = &mut u.thread(tid).queue;
    if mask & qs::TIMER != 0 {
        let msg_for = |t: &Timer| Msg {
            hwnd: t.hwnd,
            msg: if t.system { wm::SYSTIMER } else { wm::TIMER },
            wparam: t.id,
            lparam: t.proc_,
            time,
            pt: (0, 0),
        };
        let candidates: Vec<usize> = (0..q.timers.len()).filter(|&i| q.timers[i].due <= now).collect();
        let pick = candidates.into_iter().find(|&i| {
            let m = msg_for(&q.timers[i]);
            // `matches` needs the window tree, which `q` borrows from; timers
            // name their own window, so the plain comparison is enough.
            (hwnd == 0 || m.hwnd == hwnd || (hwnd == 0xffff_ffff && m.hwnd == 0))
                && (first == 0 && last == 0 || (first..=last).contains(&m.msg))
        });
        if let Some(i) = pick {
            let m = msg_for(&q.timers[i]);
            if remove {
                let t = &mut q.timers[i];
                t.due = now + t.interval_ms.max(10) as u64 * 1_000_000;
            }
            q.last_time = m.time;
            return Ok(Some(m));
        }
    }
    Ok(None)
}

/// The earliest timer expiry of this thread's queue.
fn next_timer(c: &Call<'_>) -> Option<u64> {
    let tid = c.t.tid;
    c.p.user.lock().unwrap().threads.get(&tid).and_then(|t| t.queue.timers.iter().map(|t| t.due).min())
}

/// Whether this thread's queue has anything of `mask` waiting, or `changed`
/// bits of it (MsgWaitForMultipleObjects without MWMO_INPUTAVAILABLE).
fn queue_ready(c: &Call<'_>, mask: u32, changed_only: bool) -> bool {
    let tid = c.t.tid;
    let now = now_ns(c);
    let mut u = c.p.user.lock().unwrap();
    let q = &mut u.thread(tid).queue;
    if q.timers.iter().any(|t| t.due <= now) && q.changed & qs::TIMER == 0 {
        q.changed |= qs::TIMER;
    }
    if changed_only {
        if q.changed & mask != 0 {
            q.changed &= !mask;
            return true;
        }
        return false;
    }
    let ready = (mask & (qs::KEY | qs::MOUSEMOVE | qs::MOUSEBUTTON) != 0 && !q.input.is_empty())
        || (mask & qs::SENDMESSAGE != 0 && !q.sent.is_empty())
        || (mask & (qs::POSTMESSAGE | qs::ALLPOSTMESSAGE) != 0 && (!q.posted.is_empty() || q.quit.is_some()))
        || (mask & qs::TIMER != 0 && q.timers.iter().any(|t| t.due <= now));
    ready || (mask & qs::PAINT != 0 && super::paint::window_to_paint(&u, tid, 0).is_some())
}

fn write_msg(c: &mut Call<'_>, p: u64, m: &Msg) -> R<()> {
    let abi = c.p.abi;
    macro_rules! f {
        ($n:ident) => {
            p + off!(abi, msg::$n)
        };
    }
    c.g.zero(p, off!(abi, msg::SIZEOF) as usize)?;
    c.g.wptr(f!(HWND), handle_ret(m.hwnd))?;
    c.g.w32(f!(MESSAGE), m.msg)?;
    c.g.wptr(f!(WPARAM), m.wparam)?;
    c.g.wptr(f!(LPARAM), m.lparam)?;
    c.g.w32(f!(TIME), m.time)?;
    c.g.w32(f!(PT), m.pt.0 as u32)?;
    c.g.w32(f!(PT) + 4, m.pt.1 as u32)?;
    Ok(())
}

fn read_msg(c: &Call<'_>, p: u64) -> R<Msg> {
    let abi = c.p.abi;
    macro_rules! f {
        ($n:ident) => {
            p + off!(abi, msg::$n)
        };
    }
    Ok(Msg {
        hwnd: c.g.ptr(f!(HWND))? as u32,
        msg: c.g.u32(f!(MESSAGE))?,
        wparam: c.g.ptr(f!(WPARAM))?,
        lparam: c.g.ptr(f!(LPARAM))?,
        time: c.g.u32(f!(TIME))?,
        pt: (c.g.u32(f!(PT))? as i32, c.g.u32(f!(PT) + 4)? as i32),
    })
}

/// `NtUserPeekMessage(*msg, hwnd, first, last, flags)`.
pub fn nt_user_peek_message(c: &mut Call<'_>) -> R64 {
    let (mp, hwnd, first, last, flags) = (c.a(0), c.a32(1), c.a32(2), c.a32(3), c.a32(4));
    let _ = PM_NOYIELD;
    match peek(c, hwnd, first, last, flags)? {
        Some(m) => {
            write_msg(c, mp, &m)?;
            Ok(1)
        }
        None => Ok(0),
    }
}

/// Sleep until something may have arrived in this thread's queue, or
/// `deadline` (and at the latest the next timer).
fn wait_for_queue(c: &mut Call<'_>, deadline: Option<u64>) -> R<()> {
    let t = Arc::clone(&c.t);
    let timer = next_timer(c);
    let until = match (deadline, timer) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    if c.p.terminating.load(Ordering::Relaxed) {
        return Err(Status(st::THREAD_IS_TERMINATING));
    }
    let left = until.map(|d| Duration::from_nanos(d.saturating_sub(now_ns(c))));
    if left.is_some_and(|l| l.is_zero()) {
        return Ok(());
    }
    sync::sleep_on_word(c, &t, left);
    Ok(())
}

/// `NtUserGetMessage(*msg, hwnd, first, last)`: 0 for WM_QUIT.
pub fn nt_user_get_message(c: &mut Call<'_>) -> R64 {
    let (mp, hwnd, first, last) = (c.a(0), c.a32(1), c.a32(2), c.a32(3));
    if hwnd != 0 && hwnd != 0xffff_ffff && window::full(&c.p.user.lock().unwrap(), hwnd) == 0 {
        set_last_error(c, err::INVALID_WINDOW_HANDLE);
        return Ok(u64::MAX);
    }
    let t = Arc::clone(&c.t);
    loop {
        let _ = c.g.w32(t.word, 0);
        if let Some(m) = peek(c, hwnd, first, last, PM_REMOVE)? {
            write_msg(c, mp, &m)?;
            return Ok((m.msg != wm::QUIT) as u64);
        }
        wait_for_queue(c, None)?;
    }
}

/// `NtUserWaitMessage()`.
pub fn nt_user_wait_message(c: &mut Call<'_>) -> R64 {
    let t = Arc::clone(&c.t);
    loop {
        let _ = c.g.w32(t.word, 0);
        if queue_ready(c, u32::MAX, true) || queue_ready(c, u32::MAX, false) {
            process_sent(c)?;
            return Ok(1);
        }
        wait_for_queue(c, None)?;
    }
}

/// `NtUserGetQueueStatus(flags)`: HIWORD what is there, LOWORD what is new.
pub fn nt_user_get_queue_status(c: &mut Call<'_>) -> R64 {
    let flags = c.a32(0);
    let tid = c.t.tid;
    let now = now_ns(c);
    let mut u = c.p.user.lock().unwrap();
    let q = &mut u.thread(tid).queue;
    let mut have = 0;
    if !q.posted.is_empty() || q.quit.is_some() {
        have |= qs::POSTMESSAGE | qs::ALLPOSTMESSAGE;
    }
    if !q.sent.is_empty() {
        have |= qs::SENDMESSAGE;
    }
    for hw in &q.input {
        have |= match hw.msg.msg {
            0x100..=0x109 => qs::KEY,
            0x200 => qs::MOUSEMOVE,
            _ => qs::MOUSEBUTTON,
        };
    }
    if q.timers.iter().any(|t| t.due <= now) {
        have |= qs::TIMER;
    }
    if super::paint::window_to_paint(&u, tid, 0).is_some() {
        have |= qs::PAINT;
    }
    let q = &mut u.thread(tid).queue;
    let changed = q.changed & flags;
    q.changed &= !flags;
    Ok((((have & flags) << 16) | changed) as u64)
}

/// `NtUserMsgWaitForMultipleObjectsEx(count, handles, timeout, mask, flags)`.
pub fn nt_user_msg_wait_for_multiple_objects_ex(c: &mut Call<'_>) -> R64 {
    let (count, hp, timeout, mask, flags) = (c.a32(0), c.a(1), c.a32(2), c.a32(3), c.a32(4));
    const MWMO_WAITALL: u32 = 1;
    const MWMO_ALERTABLE: u32 = 2;
    const MWMO_INPUTAVAILABLE: u32 = 4;
    if flags & MWMO_WAITALL != 0 {
        return Err(c.unimplemented("MsgWaitForMultipleObjectsEx with MWMO_WAITALL"));
    }
    if count > 63 {
        set_last_error(c, err::INVALID_PARAMETER);
        return Ok(u32::MAX as u64);
    }
    let ps = c.g.psize();
    let mut ids = Vec::new();
    for i in 0..count as u64 {
        let h = c.g.ptr(hp + i * ps)? as u32;
        ids.push(sync::wait_id(c, h)?);
    }
    // Messages already sent to us are handled first.
    process_sent(c)?;
    let deadline = (timeout != u32::MAX).then(|| now_ns(c) + timeout as u64 * 1_000_000);
    let changed_only = flags & MWMO_INPUTAVAILABLE == 0;
    let ready = move |c: &Call<'_>| queue_ready(c, mask, changed_only);
    // Timers wake the wait at their expiry: the wait is re-armed until then.
    let r = loop {
        let timer = next_timer(c);
        let until = match (deadline, timer) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let r = sync::wait_ex(c, &ids, false, flags & MWMO_ALERTABLE != 0, until, Some(&ready))?;
        if r == st::TIMEOUT && deadline.is_none_or(|d| now_ns(c) < d) {
            continue;
        }
        break r;
    };
    if r == st::USER_APC {
        crate::nt::thread::deliver_apc(c)?;
    }
    Ok(r as u64)
}

/// `NtUserPostMessage(hwnd, msg, wparam, lparam)`.
pub fn nt_user_post_message(c: &mut Call<'_>) -> R64 {
    let (h, msg, wparam, lparam) = (c.a32(0), c.a32(1), c.a(2), c.a(3));
    if h == 0xffff || h == 0xffff_ffff {
        // HWND_BROADCAST: every top-level window (the desktop's children)
        // gets one.
        let targets: Vec<(u32, u32)> = {
            let u = c.p.user.lock().unwrap();
            let kids = u.windows.get(&u.desktop).map(|d| d.children.clone()).unwrap_or_default();
            kids.iter().filter_map(|k| u.windows.get(k)).map(|w| (w.hwnd, w.tid)).collect()
        };
        let time = tick_count(c);
        for (hwnd, tid) in targets {
            post_to_thread(c, tid, Msg { hwnd, msg, wparam, lparam, time, pt: (0, 0) });
        }
        return Ok(1);
    }
    let (hwnd, tid) = if h == 0 {
        (0, c.t.tid)
    } else {
        let u = c.p.user.lock().unwrap();
        match window::find(&u, h) {
            Some(w) => (w.hwnd, w.tid),
            None => {
                drop(u);
                set_last_error(c, err::INVALID_WINDOW_HANDLE);
                return Ok(0);
            }
        }
    };
    let time = tick_count(c);
    post_to_thread(c, tid, Msg { hwnd, msg, wparam, lparam, time, pt: (0, 0) });
    Ok(1)
}

/// `NtUserPostThreadMessage(tid, msg, wparam, lparam)`.
pub fn nt_user_post_thread_message(c: &mut Call<'_>) -> R64 {
    let (tid, msg, wparam, lparam) = (c.a32(0), c.a32(1), c.a(2), c.a(3));
    if c.p.thread_by_tid(tid).is_none() {
        set_last_error(c, err::INVALID_THREAD_ID);
        return Ok(0);
    }
    let time = tick_count(c);
    post_to_thread(c, tid, Msg { hwnd: 0, msg, wparam, lparam, time, pt: (0, 0) });
    Ok(1)
}

/// `NtUserPostQuitMessage(code)`.
pub fn nt_user_post_quit_message(c: &mut Call<'_>) -> R64 {
    let code = c.a32(0) as i32 as i64 as u64;
    let tid = c.t.tid;
    let mut u = c.p.user.lock().unwrap();
    let q = &mut u.thread(tid).queue;
    q.quit = Some(code);
    q.changed |= qs::POSTMESSAGE;
    Ok(1)
}

/// `NtUserDispatchMessage(*msg)`: WM_PAINT, WM_SYSTIMER and whatever user32
/// cannot dispatch itself.
pub fn nt_user_dispatch_message(c: &mut Call<'_>) -> R64 {
    let m = read_msg(c, c.a(0))?;
    if m.hwnd == 0 {
        return Ok(0);
    }
    if (m.msg == wm::TIMER || m.msg == wm::SYSTIMER) && m.lparam != 0 {
        return Err(c.unimplemented("NtUserDispatchMessage of a timer with a TIMERPROC"));
    }
    let (owner, exists) = {
        let u = c.p.user.lock().unwrap();
        match window::find(&u, m.hwnd) {
            Some(w) => (w.tid, true),
            None => (0, false),
        }
    };
    if !exists {
        set_last_error(c, err::INVALID_WINDOW_HANDLE);
        return Ok(0);
    }
    if owner != c.t.tid {
        set_last_error(c, err::MESSAGE_SYNC_ONLY);
        return Ok(0);
    }
    let (r, _) = winproc::call_window_proc(c, m.hwnd, m.msg, m.wparam, m.lparam, &Pack::None, false, winproc::MAP_DISPATCHMESSAGE)?;
    Ok(r)
}

/// `NtUserTranslateMessage(*msg, flags)`: character messages from key
/// messages. Only key messages are translated, and there is no keyboard
/// input yet.
pub fn nt_user_translate_message(c: &mut Call<'_>) -> R64 {
    let m = read_msg(c, c.a(0))?;
    Ok(super::input::translate(c, &m)? as u64)
}

/// `NtUserSetTimer(hwnd, id, timeout, proc, tolerance)` and
/// `NtUserSetSystemTimer(hwnd, id, timeout)`.
pub fn nt_user_set_timer(c: &mut Call<'_>, system: bool) -> R64 {
    let (h, id, timeout) = (c.a32(0), c.a(1), c.a32(2));
    let proc_ = if system { 0 } else { c.a(3) };
    let tid = c.t.tid;
    let now = now_ns(c);
    let mut u = c.p.user.lock().unwrap();
    let hwnd = if h == 0 {
        0
    } else {
        match window::find(&u, h) {
            Some(w) if w.tid == tid => w.hwnd,
            Some(_) => {
                drop(u);
                return Err(c.unimplemented("SetTimer on another thread's window"));
            }
            None => {
                drop(u);
                set_last_error(c, err::INVALID_WINDOW_HANDLE);
                return Ok(0);
            }
        }
    };
    let interval = timeout.clamp(10, 0x7fff_ffff);
    let q = &mut u.thread(tid).queue;
    let id = if hwnd == 0 {
        q.next_timer_id += 1;
        0x7fff + q.next_timer_id
    } else {
        id
    };
    q.timers.retain(|t| !(t.hwnd == hwnd && t.id == id && t.system == system));
    q.timers.push(Timer { hwnd, id, interval_ms: interval, due: now + interval as u64 * 1_000_000, proc_, system });
    Ok(id)
}

/// `NtUserKillTimer(hwnd, id)` / `NtUserKillSystemTimer`.
pub fn nt_user_kill_timer(c: &mut Call<'_>, system: bool) -> R64 {
    let (h, id) = (c.a32(0), c.a(1));
    let tid = c.t.tid;
    let mut u = c.p.user.lock().unwrap();
    let hwnd = if h == 0 { 0 } else { window::full(&u, h) };
    let q = &mut u.thread(tid).queue;
    let before = q.timers.len();
    q.timers.retain(|t| !(t.hwnd == hwnd && t.id == id && t.system == system));
    // Remove WM_TIMER messages already queued for it.
    Ok((q.timers.len() != before) as u64)
}

/// `NtUserGetMessagePos()`.
pub fn nt_user_get_message_pos(c: &mut Call<'_>) -> R64 {
    let tid = c.t.tid;
    let (x, y) = c.p.user.lock().unwrap().thread(tid).queue.last_pos;
    Ok((x as u32 as u64 & 0xffff) | ((y as u32 as u64 & 0xffff) << 16))
}

/// `NtUserSetMessageExtraInfo(lparam)`: returns the previous value.
pub fn nt_user_set_message_extra_info(c: &mut Call<'_>) -> R64 {
    let v = c.a(0);
    let tid = c.t.tid;
    Ok(std::mem::replace(&mut c.p.user.lock().unwrap().thread(tid).queue.extra_info, v))
}

/// `NtUserGetThreadState(class)`.
pub fn nt_user_get_thread_state(c: &mut Call<'_>) -> R64 {
    let class = c.a32(0);
    let tid = c.t.tid;
    let mut u = c.p.user.lock().unwrap();
    let foreground = u.foreground;
    let t = u.thread(tid);
    Ok(match class {
        0 => handle_ret(t.focus),
        1 => handle_ret(t.active),
        2 => handle_ret(t.capture),
        3 | 4 => 0, // default IME window / input context: none
        5 => {
            // UserThreadStateInputState: QS_* bits of pending input
            let q = &t.queue;
            let mut bits = if q.posted.is_empty() { 0 } else { qs::POSTMESSAGE };
            for hw in &q.input {
                bits |= match hw.msg.msg {
                    0x100..=0x109 => qs::KEY,
                    0x200 => qs::MOUSEMOVE,
                    _ => qs::MOUSEBUTTON,
                };
            }
            bits as u64
        }
        6 => handle_ret(u.input.cursor_handle),
        7 => t.queue.extra_info,
        8 => (!t.queue.replying.is_empty()) as u64,
        9 => t.queue.last_time as u64,
        10 => (foreground != 0 && u.windows.get(&foreground).is_some_and(|w| w.tid == tid)) as u64,
        _ => {
            drop(u);
            return Err(c.unimplemented(&format!("NtUserGetThreadState class {class}")));
        }
    })
}

/// `NtUserMessageCall(hwnd, msg, wparam, lparam, result_info, type, ansi)`.
pub fn nt_user_message_call(c: &mut Call<'_>) -> R64 {
    let (h, msg, wparam, lparam, info, ty, ansi) = (c.a32(0), c.a32(1), c.a(2), c.a(3), c.a(4), c.a32(5), c.a32(6) != 0);
    match ty {
        // NtUserDefWindowProc
        0x029e => super::defwnd::default_window_proc(c, h, msg, wparam, lparam, ansi),
        // NtUserDesktopWindowProc: the desktop paints nothing and closes
        // nothing.
        0x029d => super::defwnd::default_window_proc(c, h, msg, wparam, lparam, ansi),
        // NtUserCallWindowProc: fill the caller's win_proc_params (func is
        // set) for user32 to call.
        0x02ab => {
            let abi = c.p.abi;
            macro_rules! f {
                ($n:ident) => {
                    info + off!(abi, struct_win_proc_params::$n)
                };
            }
            let func = c.g.ptr(f!(FUNC))?;
            if func == 0 {
                return Ok(0);
            }
            let (hwnd, dpi) = {
                let u = c.p.user.lock().unwrap();
                (window::full(&u, h), u.process_dpi_context)
            };
            let (pa, pw, ansi_dst) = winproc::winproc_params(&c.p.user.lock().unwrap(), func, ansi, ansi, true);
            c.g.wptr(f!(HWND), handle_ret(if hwnd != 0 { hwnd } else { h }))?;
            c.g.w32(f!(MSG), msg)?;
            c.g.wptr(f!(WPARAM), wparam)?;
            c.g.wptr(f!(LPARAM), lparam)?;
            c.g.w32(f!(ANSI), ansi as u32)?;
            c.g.w32(f!(ANSI_DST), ansi_dst as u32)?;
            c.g.w32(f!(MAPPING), winproc::MAP_CALLWINDOWPROC)?;
            c.g.w32(f!(DPI_CONTEXT), dpi)?;
            c.g.wptr(f!(PROCA), pa)?;
            c.g.wptr(f!(PROCW), pw)?;
            Ok(1)
        }
        // NtUserSendMessage, NtUserSendNotifyMessage
        0x02b1 | 0x02b7 => send_client_message(c, h, msg, wparam, lparam, ansi, None, ty == 0x02b7).map(|r| r.unwrap_or(0)),
        // NtUserSendMessageTimeout: send_message_timeout_params {flags,
        // timeout, result}; returns success, result in the params.
        0x02b3 => {
            let timeout = c.g.u32(info + 4)?;
            let r = send_client_message(c, h, msg, wparam, lparam, ansi, Some(timeout), false)?;
            match r {
                Some(v) => {
                    c.g.wptr(info + 8, v)?;
                    Ok(1)
                }
                None => {
                    set_last_error(c, err::TIMEOUT);
                    Ok(0)
                }
            }
        }
        // NtUserGetDispatchParams: user32 dispatches the message itself.
        0x3001 => {
            if h == 0 {
                return Ok(0);
            }
            let (exists, tid) = {
                let u = c.p.user.lock().unwrap();
                window::find(&u, h).map(|w| (true, w.tid)).unwrap_or((false, 0))
            };
            if !exists {
                set_last_error(c, err::INVALID_WINDOW_HANDLE);
                return Ok(0);
            }
            if tid != c.t.tid {
                set_last_error(c, err::MESSAGE_SYNC_ONLY);
                return Ok(0);
            }
            let p = winproc::window_call_params(c, h, msg, wparam, lparam, ansi, winproc::MAP_DISPATCHMESSAGE)
                .ok_or(Status(st::INVALID_HANDLE))?;
            c.g.write(info, &p)?;
            Ok(1)
        }
        // NtUserSpyEnter, NtUserSpyExit: message tracing, off.
        0x0303 | 0x0304 => Ok(0),
        // NtUserSpyGetMsgName: the name for debug traces.
        0x3002 => {
            let name = format!("WM_{msg:04x}");
            let n = (wparam as usize).min(name.len() + 1);
            if n > 0 {
                let mut b = name.into_bytes();
                b.push(0);
                b.truncate(n);
                let last = b.len() - 1;
                b[last] = 0;
                c.g.write(info, &b)?;
            }
            Ok(0)
        }
        _ => Err(c.unimplemented(&format!("NtUserMessageCall type {ty:#x}"))),
    }
}

/// SendMessage from the program: its own thread's window directly, another
/// thread's through that thread's queue.
#[allow(clippy::too_many_arguments)]
fn send_client_message(c: &mut Call<'_>, h: u32, msg: u32, wparam: u64, lparam: u64, ansi: bool, timeout: Option<u32>, notify: bool) -> R<Option<u64>> {
    if h == 0xffff || h == 0xffff_ffff {
        return Err(c.unimplemented("SendMessage(HWND_BROADCAST)"));
    }
    let (hwnd, tid) = {
        let u = c.p.user.lock().unwrap();
        match window::find(&u, h) {
            Some(w) => (w.hwnd, w.tid),
            None => {
                drop(u);
                set_last_error(c, err::INVALID_WINDOW_HANDLE);
                return Ok(Some(0));
            }
        }
    };
    if tid == c.t.tid {
        let (r, _) = winproc::call_window_proc(c, hwnd, msg, wparam, lparam, &Pack::None, ansi, winproc::MAP_SENDMESSAGE)?;
        return Ok(Some(r));
    }
    if notify {
        return Err(c.unimplemented("SendNotifyMessage to another thread"));
    }
    send_to_thread(c, tid, hwnd, msg, wparam, lparam, ansi, timeout)
}
