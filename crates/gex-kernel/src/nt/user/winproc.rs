//! Calling window procedures: `KeUserModeCallback(NtUserCallWinProc)` with
//! Wine's `struct win_proc_params`, and the packing of structures the NT
//! layer builds itself (CREATESTRUCTW, WINDOWPOS, …), which travel after the
//! params and are unpacked by user32's `User32CallWinProc` (Wine's
//! `pack_message`/`unpack_message` protocol).
//!
//! Messages whose lParam already points into the program's memory (what a
//! program sends itself) are passed as they are: every thread of the process
//! shares one address space.

use super::window::{self, wm, MinMax, Rect};
use super::{callbacks, class, handle_ret, message, R64};
use crate::nt::{off, thread, Abi, Call, Status, R};

/// `KeUserModeCallback(id, args)`.
pub fn user_callback(c: &mut Call<'_>, id: u32, args: &[u8]) -> R<(u32, Vec<u8>)> {
    thread::user_callback(c, id, args)
}

#[derive(Clone, Debug)]
pub struct CreateStruct {
    pub params: u64,
    pub instance: u64,
    pub menu: u64,
    pub parent: u64,
    pub cx: i32,
    pub cy: i32,
    pub x: i32,
    pub y: i32,
    pub style: u32,
    /// The name; `[0xffff, id]` is an ordinal.
    pub name: Option<Vec<u16>>,
    /// The caller's pointer to the name, passed on as it is.
    pub name_ptr: u64,
    pub class: super::NameOrAtom,
    pub ex_style: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct WindowPos {
    pub hwnd: u32,
    pub after: u32,
    pub x: i32,
    pub y: i32,
    pub cx: i32,
    pub cy: i32,
    pub flags: u32,
}

/// What travels after the params.
#[derive(Clone, Debug)]
pub enum Pack {
    None,
    Create(CreateStruct),
    /// WM_NCCALCSIZE with wparam FALSE: a RECT.
    Rect(Rect),
    /// WM_NCCALCSIZE with wparam TRUE: NCCALCSIZE_PARAMS then WINDOWPOS.
    NcCalc([Rect; 3], WindowPos),
    WindowPos(WindowPos),
    MinMax([(i32, i32); 5]),
    Style(u32, u32),
}

fn put_ptr(b: &mut Vec<u8>, at: usize, v: u64, abi: Abi) {
    let n = abi.ptr_size() as usize;
    if b.len() < at + n {
        b.resize(at + n, 0);
    }
    b[at..at + n].copy_from_slice(&v.to_le_bytes()[..n]);
}

fn put32(b: &mut Vec<u8>, at: usize, v: u32) {
    if b.len() < at + 4 {
        b.resize(at + 4, 0);
    }
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn get32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map(|x| u32::from_le_bytes(x.try_into().unwrap())).unwrap_or(0)
}

fn put_rect(b: &mut Vec<u8>, at: usize, r: &Rect) {
    put32(b, at, r.left as u32);
    put32(b, at + 4, r.top as u32);
    put32(b, at + 8, r.right as u32);
    put32(b, at + 12, r.bottom as u32);
}

fn get_rect(b: &[u8], at: usize) -> Rect {
    Rect { left: get32(b, at) as i32, top: get32(b, at + 4) as i32, right: get32(b, at + 8) as i32, bottom: get32(b, at + 12) as i32 }
}

fn encode_windowpos(b: &mut Vec<u8>, at: usize, p: &WindowPos, abi: Abi) {
    let o = |f: u64| at + f as usize;
    put_ptr(b, o(off!(abi, windowpos::HWND)), handle_ret(p.hwnd), abi);
    put_ptr(b, o(off!(abi, windowpos::HWNDINSERTAFTER)), handle_ret(p.after), abi);
    put32(b, o(off!(abi, windowpos::X)), p.x as u32);
    put32(b, o(off!(abi, windowpos::Y)), p.y as u32);
    put32(b, o(off!(abi, windowpos::CX)), p.cx as u32);
    put32(b, o(off!(abi, windowpos::CY)), p.cy as u32);
    put32(b, o(off!(abi, windowpos::FLAGS)), p.flags);
    b.resize(b.len().max(at + off!(abi, windowpos::SIZEOF) as usize), 0);
}

fn decode_windowpos(b: &[u8], at: usize, base: &WindowPos, abi: Abi) -> WindowPos {
    let o = |f: u64| at + f as usize;
    WindowPos {
        hwnd: base.hwnd,
        after: get32(b, o(off!(abi, windowpos::HWNDINSERTAFTER))),
        x: get32(b, o(off!(abi, windowpos::X))) as i32,
        y: get32(b, o(off!(abi, windowpos::Y))) as i32,
        cx: get32(b, o(off!(abi, windowpos::CX))) as i32,
        cy: get32(b, o(off!(abi, windowpos::CY))) as i32,
        flags: get32(b, o(off!(abi, windowpos::FLAGS))),
    }
}

fn pack(p: &Pack, abi: Abi) -> Vec<u8> {
    let mut b = Vec::new();
    match p {
        Pack::None => {}
        Pack::Create(cs) => {
            let o = |f: u64| f as usize;
            put_ptr(&mut b, o(off!(abi, createstructw::LPCREATEPARAMS)), cs.params, abi);
            put_ptr(&mut b, o(off!(abi, createstructw::HINSTANCE)), cs.instance, abi);
            put_ptr(&mut b, o(off!(abi, createstructw::HMENU)), cs.menu, abi);
            put_ptr(&mut b, o(off!(abi, createstructw::HWNDPARENT)), cs.parent, abi);
            put32(&mut b, o(off!(abi, createstructw::CY)), cs.cy as u32);
            put32(&mut b, o(off!(abi, createstructw::CX)), cs.cx as u32);
            put32(&mut b, o(off!(abi, createstructw::Y)), cs.y as u32);
            put32(&mut b, o(off!(abi, createstructw::X)), cs.x as u32);
            put32(&mut b, o(off!(abi, createstructw::STYLE)), cs.style);
            put32(&mut b, o(off!(abi, createstructw::DWEXSTYLE)), cs.ex_style);
            b.resize(off!(abi, createstructw::SIZEOF) as usize, 0);
            // A string pointer is anything above 0xffff; user32 points it at
            // the copy that follows.
            let mut strings = Vec::new();
            let name_field = match &cs.name {
                Some(n) if n.first() == Some(&0xffff) && n.len() == 2 => n[1] as u64,
                Some(n) => {
                    strings.extend(n.iter().flat_map(|c| c.to_le_bytes()));
                    strings.extend([0, 0]);
                    0x1_0000
                }
                None => 0,
            };
            let class_field = match &cs.class {
                super::NameOrAtom::Atom(a) => *a as u64,
                super::NameOrAtom::Name(n) => {
                    strings.extend(n.iter().flat_map(|c| c.to_le_bytes()));
                    strings.extend([0, 0]);
                    0x1_0000
                }
            };
            put_ptr(&mut b, o(off!(abi, createstructw::LPSZNAME)), name_field, abi);
            put_ptr(&mut b, o(off!(abi, createstructw::LPSZCLASS)), class_field, abi);
            b.extend(strings);
        }
        Pack::Rect(r) => put_rect(&mut b, 0, r),
        Pack::NcCalc(rects, pos) => {
            for (i, r) in rects.iter().enumerate() {
                put_rect(&mut b, 16 * i, r);
            }
            // lppos is fixed up by user32; NCCALCSIZE_PARAMS is 3 RECTs and
            // a pointer.
            let ncsize = off!(abi, nccalcsize_params::SIZEOF) as usize;
            put_ptr(&mut b, off!(abi, nccalcsize_params::LPPOS) as usize, 0x1_0000, abi);
            b.resize(ncsize, 0);
            encode_windowpos(&mut b, ncsize, pos, abi);
        }
        Pack::WindowPos(pos) => encode_windowpos(&mut b, 0, pos, abi),
        Pack::MinMax(pts) => {
            for (i, (x, y)) in pts.iter().enumerate() {
                put32(&mut b, 8 * i, *x as u32);
                put32(&mut b, 8 * i + 4, *y as u32);
            }
        }
        Pack::Style(old, new) => {
            put32(&mut b, 0, *old);
            put32(&mut b, 4, *new);
        }
    }
    b
}

/// The `win_proc_params` for calling `hwnd`'s procedure, as win32u's
/// `init_window_call_params` fills them.
pub fn window_call_params(c: &Call<'_>, hwnd: u32, msg: u32, wparam: u64, lparam: u64, ansi: bool, mapping: u32) -> Option<Vec<u8>> {
    let u = c.p.user.lock().unwrap();
    let w = window::find(&u, hwnd)?;
    let func = w.wndproc;
    let ansi_dst = !w.unicode;
    let dpi = u.process_dpi_context;
    let (proc_a, proc_w, ansi_dst) = winproc_params(&u, func, ansi, ansi_dst, true);
    let abi = c.p.abi;
    let mut b = Vec::new();
    macro_rules! f {
        ($n:ident) => {
            off!(abi, struct_win_proc_params::$n) as usize
        };
    }
    put_ptr(&mut b, f!(FUNC), func, abi);
    put_ptr(&mut b, f!(HWND), handle_ret(w.hwnd), abi);
    put32(&mut b, f!(MSG), msg);
    put_ptr(&mut b, f!(WPARAM), wparam, abi);
    put_ptr(&mut b, f!(LPARAM), lparam, abi);
    put32(&mut b, f!(ANSI), ansi as u32);
    put32(&mut b, f!(ANSI_DST), ansi_dst as u32);
    put32(&mut b, f!(MAPPING), mapping);
    put32(&mut b, f!(DPI_CONTEXT), dpi);
    put_ptr(&mut b, f!(PROCA), proc_a, abi);
    put_ptr(&mut b, f!(PROCW), proc_w, abi);
    b.resize(off!(abi, struct_win_proc_params::SIZEOF) as usize, 0);
    Some(b)
}

/// Wine's `get_winproc_params`: (procA, procW, ansi_dst).
pub fn winproc_params(u: &super::User, func: u64, ansi: bool, ansi_dst: bool, fixup: bool) -> (u64, u64, bool) {
    let mut ansi_dst = ansi_dst;
    let (mut a, mut w) = match class::winproc_index(u, func) {
        None => (0, 0),
        Some(i) => {
            let (a, w) = u.winprocs[i];
            if fixup {
                if ansi {
                    if a != 0 {
                        ansi_dst = true;
                    } else if w != 0 {
                        ansi_dst = false;
                    }
                } else if w != 0 {
                    ansi_dst = false;
                } else if a != 0 {
                    ansi_dst = true;
                }
            }
            (a, w)
        }
    };
    if a == 0 {
        a = func;
    }
    if w == 0 {
        w = func;
    }
    (a, w, ansi_dst)
}

/// `WMCHAR_MAP_SENDMESSAGE`, `WMCHAR_MAP_DISPATCHMESSAGE`,
/// `WMCHAR_MAP_CALLWINDOWPROC`.
pub const MAP_SENDMESSAGE: u32 = 1;
pub const MAP_DISPATCHMESSAGE: u32 = 4;
pub const MAP_CALLWINDOWPROC: u32 = 5;

/// Call `hwnd`'s window procedure on this thread. Returns the LRESULT and
/// the packed data as the procedure left it.
pub fn call_window_proc(c: &mut Call<'_>, hwnd: u32, msg: u32, wparam: u64, lparam: u64, p: &Pack, ansi: bool, mapping: u32) -> R<(u64, Vec<u8>)> {
    let Some(params) = window_call_params(c, hwnd, msg, wparam, lparam, ansi, mapping) else {
        return Ok((0, Vec::new()));
    };
    call_with_params(c, params, p)
}

/// Run a `win_proc_params` through user32, with `p` packed after it.
pub fn call_with_params(c: &mut Call<'_>, mut args: Vec<u8>, p: &Pack) -> R<(u64, Vec<u8>)> {
    let abi = c.p.abi;
    let packed = pack(p, abi);
    if !packed.is_empty() {
        args.resize((args.len() + 15) & !15, 0);
        args.extend_from_slice(&packed);
    }
    {
        let tid = c.t.tid;
        let mut u = c.p.user.lock().unwrap();
        let t = u.thread(tid);
        if t.call_depth > 64 {
            return Ok((0, Vec::new()));
        }
        t.call_depth += 1;
    }
    let r = user_callback(c, callbacks::CALL_WIN_PROC, &args);
    {
        let tid = c.t.tid;
        let mut u = c.p.user.lock().unwrap();
        let t = u.thread(tid);
        t.call_depth = t.call_depth.saturating_sub(1);
    }
    let (_status, data) = r?;
    let ps = abi.ptr_size() as usize;
    let mut v = [0u8; 8];
    let n = data.len().min(ps);
    v[..n].copy_from_slice(&data[..n]);
    let result = match abi {
        Abi::P64 => u64::from_le_bytes(v),
        Abi::P32 => u32::from_le_bytes(v[..4].try_into().unwrap()) as i32 as i64 as u64,
    };
    Ok((result, data.get(ps..).map(|d| d.to_vec()).unwrap_or_default()))
}

/// SendMessage from the NT layer to one of the process's windows: a direct
/// call on this thread, or a sent message to the owning thread.
pub fn send_message(c: &mut Call<'_>, hwnd: u32, msg: u32, wparam: u64, lparam: u64, p: Pack) -> R<u64> {
    Ok(send_message_packed(c, hwnd, msg, wparam, lparam, p)?.0)
}

pub fn send_message_packed(c: &mut Call<'_>, hwnd: u32, msg: u32, wparam: u64, lparam: u64, p: Pack) -> R<(u64, Vec<u8>)> {
    let tid = {
        let u = c.p.user.lock().unwrap();
        match window::find(&u, hwnd) {
            Some(w) => w.tid,
            None => return Ok((0, Vec::new())),
        }
    };
    if tid == c.t.tid {
        return call_window_proc(c, hwnd, msg, wparam, lparam, &p, false, MAP_SENDMESSAGE);
    }
    if !matches!(p, Pack::None) {
        return Err(c.unimplemented(&format!("a packed message {msg:#x} to another thread's window")));
    }
    Ok((message::send_to_thread(c, tid, hwnd, msg, wparam, lparam, false, None)?.unwrap_or(0), Vec::new()))
}

/// WM_NCCALCSIZE with wparam FALSE: the client rectangle of `screen`.
pub fn send_nccalcsize(c: &mut Call<'_>, hwnd: u32, screen: Rect) -> R<(u64, Rect)> {
    let (r, data) = send_message_packed(c, hwnd, wm::NCCALCSIZE, 0, 0, Pack::Rect(screen))?;
    Ok((r, if data.len() >= 16 { get_rect(&data, 0) } else { screen }))
}

/// WM_NCCALCSIZE with wparam TRUE during SetWindowPos: the new client
/// rectangle (parent coordinates).
pub fn send_nccalcsize_valid(c: &mut Call<'_>, hwnd: u32, new_wr: Rect, old_wr: Rect, old_cr: Rect, pos: &WindowPos) -> R<(u64, Rect)> {
    let (r, data) = send_message_packed(c, hwnd, wm::NCCALCSIZE, 1, 0, Pack::NcCalc([new_wr, old_wr, old_cr], *pos))?;
    Ok((r, if data.len() >= 16 { get_rect(&data, 0) } else { new_wr }))
}

/// WM_WINDOWPOSCHANGING / WM_WINDOWPOSCHANGED; returns the WINDOWPOS as the
/// procedure left it.
pub fn send_windowpos(c: &mut Call<'_>, hwnd: u32, msg: u32, pos: WindowPos) -> R<WindowPos> {
    let (_, data) = send_message_packed(c, hwnd, msg, 0, 0, Pack::WindowPos(pos))?;
    Ok(if data.is_empty() { pos } else { decode_windowpos(&data, 0, &pos, c.p.abi) })
}

/// WM_GETMINMAXINFO.
pub fn send_getminmaxinfo(c: &mut Call<'_>, hwnd: u32, info: MinMax) -> R<MinMax> {
    let pts = [info.reserved, info.max_size, info.max_pos, info.min_track, info.max_track];
    let (_, data) = send_message_packed(c, hwnd, wm::GETMINMAXINFO, 0, 0, Pack::MinMax(pts))?;
    if data.len() < 40 {
        return Ok(info);
    }
    let pt = |i: usize| (get32(&data, 8 * i) as i32, get32(&data, 8 * i + 4) as i32);
    Ok(MinMax { reserved: pt(0), max_size: pt(1), max_pos: pt(2), min_track: pt(3), max_track: pt(4) })
}

/// WM_STYLECHANGING / WM_STYLECHANGED; returns the new style as left.
pub fn send_stylechange(c: &mut Call<'_>, hwnd: u32, msg: u32, which: i32, old: u32, new: u32) -> R<u32> {
    let (_, data) = send_message_packed(c, hwnd, msg, which as i64 as u64, 0, Pack::Style(old, new))?;
    Ok(if data.len() >= 8 { get32(&data, 4) } else { new })
}

#[allow(dead_code)]
fn _unused(_: Status) -> R64 {
    Ok(0)
}
