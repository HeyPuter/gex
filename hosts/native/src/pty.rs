//! Pseudoterminals, natively: `/dev/ptmx` and `/dev/pts`.
//!
//! The kernel reaches a pty pair through [`gex_kernel::pty::Remote`], a table
//! of calls by number, because in the page the pair lives in the global
//! kernel module (`hosts/kernel`, `k_pty_*`). Natively every process shares
//! this host process, so the pairs live here, in one table, and the calls are
//! these. The same [`Pty`] does the work in both: the line discipline, echo,
//! termios, window size, foreground group and hangup.
//!
//! What differs from the page: an end is counted per handle, not per process.
//! In the page each process has a memory and a descriptor table of its own, so
//! the global kernel counts holders by pid; natively a forked child shares its
//! parent's open files, so a handle is one open of an end, and dropping it is
//! that open's close.

use std::sync::{Arc, Mutex, OnceLock};

use gex_kernel::pty::{Pty, Remote};
use gex_kernel::proc::ProcTable;

fn table() -> &'static Mutex<Vec<Arc<Pty>>> {
    static T: Mutex<Vec<Arc<Pty>>> = Mutex::new(Vec::new());
    &T
}

static PROCS: OnceLock<Arc<ProcTable>> = OnceLock::new();

fn at(id: i32) -> Option<Arc<Pty>> {
    let t = table().lock().unwrap_or_else(|e| e.into_inner());
    t.get(usize::try_from(id).ok()?).cloned()
}

/// A signal to the pair's foreground group, if one has claimed it: `^C`,
/// hangup and `SIGWINCH` reach the slave's programs this way.
fn signal(p: &Pty, sig: u32) {
    let fg = p.tty.fg();
    if let (true, Some(procs)) = (fg > 0, PROCS.get()) {
        let _ = procs.kill_group(gex_kernel::INIT_PID, fg, sig);
    }
}

/// The master's end of file is real once the session that claimed the slave
/// has gone; before any session claimed it, the last close is.
fn hangup_is_real(p: &Pty) -> bool {
    let sid = p.tty.sid();
    match (sid > 0, PROCS.get()) {
        (true, Some(procs)) => !procs.alive(sid),
        _ => true,
    }
}

extern "C" fn open_master() -> i32 {
    let mut t = table().lock().unwrap_or_else(|e| e.into_inner());
    let id = t.len() as i32;
    if id > 0xffff {
        return -1;
    }
    t.push(Arc::new(Pty::new(id as u32)));
    (id << 16) | id
}

extern "C" fn open_slave(ptn: i32) -> i32 {
    let Some(p) = at(ptn) else { return -1 };
    if p.locked() {
        return -5; // EIO: unlockpt was not called
    }
    p.open_slave();
    ptn
}

extern "C" fn open_end(id: i32, master: i32) {
    if let Some(p) = at(id) {
        if master != 0 {
            p.open_master();
        } else {
            p.open_slave();
        }
    }
}

extern "C" fn close_end(id: i32, master: i32) {
    if let Some(p) = at(id) {
        if master != 0 {
            if p.close_master() {
                signal(&p, gex_kernel::signal::sig::SIGHUP);
            }
        } else {
            p.close_slave();
        }
    }
    crate::ready::notify();
}

fn read(id: i32, master: i32, buf: &mut [u8]) -> i32 {
    let Some(p) = at(id) else { return -9 };
    let r = if master != 0 {
        match p.master_read(buf) {
            Err(e) if e == gex_kernel::errno::Errno::EIO && !hangup_is_real(&p) => Err(gex_kernel::errno::Errno::EAGAIN),
            other => other,
        }
    } else {
        p.slave_read(buf)
    };
    match r {
        Ok(n) => n as i32,
        Err(e) => -(e.0 as i32),
    }
}

fn write(id: i32, master: i32, buf: &[u8]) -> i32 {
    let Some(p) = at(id) else { return -9 };
    if master != 0 {
        for sig in p.master_write(buf) {
            signal(&p, sig);
        }
    } else {
        p.slave_write(buf);
    }
    crate::ready::notify();
    buf.len() as i32
}

extern "C" fn readable(id: i32, master: i32) -> i32 {
    let Some(p) = at(id) else { return 1 };
    if master != 0 {
        if p.master_available() > 0 {
            return 1;
        }
        return (p.master_readable() && hangup_is_real(&p)) as i32;
    }
    p.slave_readable() as i32
}

extern "C" fn available(id: i32, master: i32) -> i32 {
    let Some(p) = at(id) else { return 0 };
    (if master != 0 { p.master_available() } else { p.slave_available() }) as i32
}

extern "C" fn ptn(id: i32) -> i32 {
    at(id).map_or(-1, |p| p.ptn() as i32)
}

extern "C" fn get_lock(id: i32) -> i32 {
    at(id).map_or(1, |p| p.locked() as i32)
}

extern "C" fn set_lock(id: i32, locked: i32) {
    if let Some(p) = at(id) {
        p.set_locked(locked != 0);
    }
}

fn get_termios(id: i32, buf: &mut [u8]) -> i32 {
    let Some(p) = at(id) else { return -1 };
    let t = p.tty.termios();
    let n = buf.len().min(t.len());
    buf[..n].copy_from_slice(&t[..n]);
    n as i32
}

fn set_termios(id: i32, src: &[u8], flush: i32) -> i32 {
    let Some(p) = at(id) else { return -1 };
    if flush != 0 {
        p.flush(gex_kernel::tty::TCIFLUSH);
    }
    p.set_termios(src);
    0
}

extern "C" fn winsize(id: i32) -> u64 {
    at(id).map_or(0, |p| gex_kernel::tty::pack_ws(p.tty.winsize()))
}

extern "C" fn set_winsize(id: i32, packed: u64) -> i32 {
    let Some(p) = at(id) else { return 0 };
    let changed = p.tty.set_winsize(gex_kernel::tty::unpack_ws(packed));
    if changed {
        signal(&p, gex_kernel::signal::sig::SIGWINCH);
    }
    changed as i32
}

extern "C" fn for_sid(sid: i32) -> i32 {
    if sid <= 0 {
        return -1;
    }
    let t = table().lock().unwrap_or_else(|e| e.into_inner());
    t.iter().position(|p| p.tty.sid() == sid).map_or(-1, |i| i as i32)
}

extern "C" fn fg(id: i32) -> i32 {
    at(id).map_or(0, |p| p.tty.fg())
}

extern "C" fn set_fg(id: i32, pgrp: i32) {
    if let Some(p) = at(id) {
        p.tty.set_fg(pgrp);
        p.notify();
    }
    crate::ready::notify();
}

extern "C" fn sid(id: i32) -> i32 {
    at(id).map_or(0, |p| p.tty.sid())
}

extern "C" fn set_sid(id: i32, s: i32) {
    if let Some(p) = at(id) {
        p.tty.set_sid(s);
    }
}

extern "C" fn flush(id: i32, which: i32) {
    if let Some(p) = at(id) {
        p.flush(which);
    }
}

extern "C" fn queue(id: i32) -> u32 {
    at(id).map_or(0, |p| p.queue_id())
}

/// The calls, for `Kernel::ptys`, with `procs` the run's process table (whose
/// groups the terminal's signals go to).
pub fn remote(procs: Arc<ProcTable>) -> Remote {
    let _ = PROCS.set(procs);
    Remote {
        open_master,
        open_slave,
        open_end,
        close_end,
        read,
        write,
        readable,
        available,
        ptn,
        get_lock,
        set_lock,
        get_termios,
        set_termios,
        winsize,
        set_winsize,
        fg,
        for_sid,
        set_fg,
        sid,
        set_sid,
        flush,
        queue,
    }
}

/// The pts numbers some handle still holds an end of: what `/dev/pts` lists.
pub fn in_use() -> Vec<u32> {
    let t = table().lock().unwrap_or_else(|e| e.into_inner());
    t.iter()
        .enumerate()
        .filter(|(_, p)| p.master_count() + p.slave_count() > 0)
        .map(|(i, _)| i as u32)
        .collect()
}
