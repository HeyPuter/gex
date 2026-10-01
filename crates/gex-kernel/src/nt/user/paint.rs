//! Painting: window DCs, update regions, WM_PAINT and WM_ERASEBKGND
//! (win32u's dce.c).
//!
//! A window's update region is kept as its bounding rectangle in client
//! coordinates, which over-paints but never under-paints: Windows promises
//! only that everything invalid gets a WM_PAINT. The non-client area is not
//! drawn by the NT layer (the compositor decorates windows, §6a.1), so
//! WM_NCPAINT is never needed.

use super::message;
use super::window::{self, wm, ws, Rect};
use super::winproc::{self, Pack};
use super::{err, handle_ret, read_rect, set_last_error, write_rect, User, R64};
use crate::nt::gdi::{self, dc as gdc, Obj};
use crate::nt::{off, Call, R};

/// `DCX_*`.
pub mod dcx {
    pub const WINDOW: u32 = 0x1;
    pub const CACHE: u32 = 0x2;
    pub const NORESETATTRS: u32 = 0x4;
    pub const CLIPCHILDREN: u32 = 0x8;
    pub const CLIPSIBLINGS: u32 = 0x10;
    pub const PARENTCLIP: u32 = 0x20;
    pub const EXCLUDERGN: u32 = 0x40;
    pub const INTERSECTRGN: u32 = 0x80;
    pub const USESTYLE: u32 = 0x10000;
}

/// `RDW_*`.
pub mod rdw {
    pub const INVALIDATE: u32 = 0x1;
    pub const INTERNALPAINT: u32 = 0x2;
    pub const ERASE: u32 = 0x4;
    pub const VALIDATE: u32 = 0x8;
    pub const NOINTERNALPAINT: u32 = 0x10;
    pub const NOERASE: u32 = 0x20;
    pub const NOCHILDREN: u32 = 0x40;
    pub const ALLCHILDREN: u32 = 0x80;
    pub const UPDATENOW: u32 = 0x100;
    pub const ERASENOW: u32 = 0x200;
    pub const FRAME: u32 = 0x400;
    pub const NOFRAME: u32 = 0x800;
}

const CS_OWNDC: u32 = 0x20;
const CS_CLASSDC: u32 = 0x40;

/// The client rectangle of `w` in its own client coordinates.
fn client_area(w: &window::Window) -> Rect {
    Rect { left: 0, top: 0, right: w.client_rect.width(), bottom: w.client_rect.height() }
}

/// Add `r` (client coordinates; `None` for all of it) to `hwnd`'s update
/// region, and to its children's when `children` says so.
pub fn invalidate(c: &mut Call<'_>, hwnd: u32, r: Option<Rect>, erase: bool, children: bool) {
    let mut wake = Vec::new();
    {
        let mut u = c.p.user.lock().unwrap();
        invalidate_locked(&mut u, hwnd, r, erase, children, &mut wake);
    }
    for tid in wake {
        message::wake_for_paint(c, tid);
    }
}

fn invalidate_locked(u: &mut User, hwnd: u32, r: Option<Rect>, erase: bool, children: bool, wake: &mut Vec<u32>) {
    let Some(w) = u.windows.get_mut(&hwnd) else { return };
    let area = client_area(w);
    let r = r.map(|r| r.intersect(&area)).unwrap_or(area);
    if !r.is_empty() && w.style & ws::VISIBLE != 0 {
        w.update = w.update.union(&r);
        w.erase |= erase;
        if !wake.contains(&w.tid) {
            wake.push(w.tid);
        }
    }
    if !children {
        return;
    }
    let (kids, origin) = (w.children.clone(), (w.client_rect.left, w.client_rect.top));
    let _ = origin;
    for k in kids {
        // A child's part of `r`, in the child's client coordinates.
        let Some(cw) = u.windows.get(&k) else { continue };
        let cr = cw.client_rect;
        let part = r.intersect(&cr).offset(-cr.left, -cr.top);
        if !part.is_empty() {
            invalidate_locked(u, k, Some(part), erase, true, wake);
        }
    }
}

/// Remove `r` (client coordinates; `None` for all) from the update region.
/// A partial validation keeps the bounding rectangle of what is left.
pub fn validate(c: &mut Call<'_>, hwnd: u32, r: Option<Rect>, children: bool) {
    let mut u = c.p.user.lock().unwrap();
    validate_locked(&mut u, hwnd, r, children);
}

fn validate_locked(u: &mut User, hwnd: u32, r: Option<Rect>, children: bool) {
    let Some(w) = u.windows.get_mut(&hwnd) else { return };
    match r {
        None => {
            w.update = Rect::default();
            w.erase = false;
        }
        Some(r) => {
            let left = gdi::region::subtract(&[w.update], &[r]);
            w.update = gdi::region::bounds(&left);
            if w.update.is_empty() {
                w.erase = false;
            }
        }
    }
    if children {
        for k in w.children.clone() {
            let Some(cw) = u.windows.get(&k) else { continue };
            let cr = cw.client_rect;
            let part = r.map(|r| r.intersect(&cr).offset(-cr.left, -cr.top));
            if part.is_none_or(|p| !p.is_empty()) {
                validate_locked(u, k, part, true);
            }
        }
    }
}

/// The window of thread `tid` that needs a WM_PAINT, parents before
/// children, among `hwnd` and its descendants (0: all of the thread's).
pub fn window_to_paint(u: &User, tid: u32, hwnd: u32) -> Option<u32> {
    fn walk(u: &User, h: u32, tid: u32) -> Option<u32> {
        let w = u.windows.get(&h)?;
        if w.style & ws::VISIBLE == 0 {
            return None;
        }
        if w.tid == tid && (!w.update.is_empty() || w.internal_paint) {
            return Some(h);
        }
        w.children.iter().rev().find_map(|&k| walk(u, k, tid))
    }
    if hwnd != 0 {
        return walk(u, hwnd, tid);
    }
    let desktop = u.windows.get(&u.desktop)?;
    desktop.children.iter().rev().find_map(|&k| walk(u, k, tid))
}

/// Get a DC for a window (Wine's `NtUserGetDCEx`).
pub fn get_dc_ex(c: &mut Call<'_>, h: u32, clip_rgn: u32, flags: u32) -> R<u32> {
    window::get_desktop_window(c)?;
    let (hwnd, style, cls_style, own_dc, desktop) = {
        let u = c.p.user.lock().unwrap();
        let hwnd = if h == 0 { u.desktop } else { window::full(&u, h) };
        let Some(w) = u.windows.get(&hwnd) else { return Ok(0) };
        (hwnd, w.style, window::class_of(&u, w).style, w.own_dc, u.desktop)
    };
    if hwnd == 0 {
        return Ok(0);
    }
    if flags & (dcx::INTERSECTRGN | dcx::EXCLUDERGN) != 0 && clip_rgn != 0 {
        return Err(c.unimplemented("GetDCEx with DCX_INTERSECTRGN/DCX_EXCLUDERGN"));
    }
    let whole = flags & dcx::WINDOW != 0;
    let _ = style;
    gdi::ensure_init(c)?;
    if cls_style & CS_CLASSDC != 0 && hwnd != desktop && flags & (dcx::CACHE | dcx::WINDOW) == 0 {
        return Err(c.unimplemented("a CS_CLASSDC window's DC"));
    }
    if cls_style & CS_OWNDC != 0 && hwnd != desktop && flags & (dcx::CACHE | dcx::WINDOW) == 0 {
        if own_dc != 0 {
            with_dc(c, own_dc, |d| d.clip = None);
            return Ok(own_dc);
        }
        let hdc = gdc::create(c, false, hwnd, false)?;
        set_vis_rect(c, hdc, hwnd, false)?;
        if let Some(w) = c.p.user.lock().unwrap().windows.get_mut(&hwnd) {
            w.own_dc = hdc;
        }
        return Ok(hdc);
    }
    let hdc = gdc::create(c, false, hwnd, whole)?;
    set_vis_rect(c, hdc, hwnd, whole)?;
    Ok(hdc)
}

fn with_dc(c: &Call<'_>, hdc: u32, f: impl FnOnce(&mut gdc::Dc)) {
    gdi::with_obj(c, hdc, |o| {
        if let Obj::Dc(d) = o {
            f(d);
        }
    });
}

/// A window DC's DC_ATTR vis_rect: its drawable extent.
fn set_vis_rect(c: &mut Call<'_>, hdc: u32, hwnd: u32, whole: bool) -> R<()> {
    let r = {
        let u = c.p.user.lock().unwrap();
        match u.windows.get(&hwnd) {
            Some(w) if hwnd != u.desktop => {
                let r = if whole { w.window_rect } else { w.client_rect };
                Rect { left: 0, top: 0, right: r.width(), bottom: r.height() }
            }
            _ => Rect { left: 0, top: 0, right: 1920, bottom: 1080 },
        }
    };
    let Some(Obj::Dc(d)) = gdi::get(c, hdc) else { return Ok(()) };
    write_rect(&mut c.g, d.attr + off!(c.p.abi, dc_attr::VIS_RECT), &r)
}

/// `NtUserGetDCEx(hwnd, clip_rgn, flags)`.
pub fn nt_user_get_dc_ex(c: &mut Call<'_>) -> R64 {
    let (h, rgn, flags) = (c.a32(0), c.a32(1), c.a32(2));
    Ok(handle_ret(get_dc_ex(c, h, rgn, flags)?))
}

/// `NtUserGetDC(hwnd)`.
pub fn nt_user_get_dc(c: &mut Call<'_>) -> R64 {
    let h = c.a32(0);
    let flags = if h == 0 { dcx::CACHE | dcx::WINDOW } else { dcx::USESTYLE };
    Ok(handle_ret(get_dc_ex(c, h, 0, flags)?))
}

/// `NtUserGetWindowDC(hwnd)`.
pub fn nt_user_get_window_dc(c: &mut Call<'_>) -> R64 {
    let h = c.a32(0);
    Ok(handle_ret(get_dc_ex(c, h, 0, dcx::USESTYLE | dcx::WINDOW)?))
}

/// Release a DC from GetDC: a window's own DC stays; a cache DC goes.
pub fn release_dc(c: &mut Call<'_>, hdc: u32) -> R<bool> {
    let Some(Obj::Dc(d)) = gdi::get(c, hdc) else { return Ok(false) };
    if d.memory {
        return Ok(false);
    }
    let own = c.p.user.lock().unwrap().windows.values().any(|w| w.own_dc != 0 && w.own_dc & 0xffff == hdc & 0xffff);
    if own {
        return Ok(true);
    }
    if let Some(Obj::Dc(d)) = gdi::free(c, hdc)? {
        gdc::release(c, d)?;
    }
    Ok(true)
}

/// `NtUserReleaseDC(hwnd, hdc)`.
pub fn nt_user_release_dc(c: &mut Call<'_>) -> R64 {
    let hdc = c.a32(1);
    Ok(release_dc(c, hdc)? as u64)
}

/// `NtUserWindowFromDC(hdc)`.
pub fn nt_user_window_from_dc(c: &mut Call<'_>) -> R64 {
    let hdc = c.a32(0);
    Ok(match gdi::get(c, hdc) {
        Some(Obj::Dc(d)) if !d.memory => handle_ret(d.hwnd),
        _ => 0,
    })
}

/// Send WM_ERASEBKGND if the window still owes one; true if it erased.
fn send_erase(c: &mut Call<'_>, hwnd: u32, hdc: u32) -> R<bool> {
    let owed = {
        let mut u = c.p.user.lock().unwrap();
        match u.windows.get_mut(&hwnd) {
            Some(w) if w.erase => {
                w.erase = false;
                true
            }
            _ => false,
        }
    };
    if !owed {
        return Ok(true);
    }
    Ok(winproc::send_message(c, hwnd, wm::ERASEBKGND, hdc as u64, 0, Pack::None)? != 0)
}

/// `NtUserBeginPaint(hwnd, ps)`.
pub fn nt_user_begin_paint(c: &mut Call<'_>) -> R64 {
    let (h, ps) = (c.a32(0), c.a(1));
    let hwnd = window::full(&c.p.user.lock().unwrap(), h);
    if hwnd == 0 {
        set_last_error(c, err::INVALID_WINDOW_HANDLE);
        return Ok(0);
    }
    let (update, erase) = {
        let mut u = c.p.user.lock().unwrap();
        let w = u.windows.get_mut(&hwnd).unwrap();
        let r = (w.update, w.erase);
        w.update = Rect::default();
        w.internal_paint = false;
        r
    };
    let hdc = get_dc_ex(c, hwnd, 0, dcx::USESTYLE)?;
    with_dc(c, hdc, |d| d.clip = Some(if update.is_empty() { Vec::new() } else { vec![update] }));
    if erase {
        if let Some(w) = c.p.user.lock().unwrap().windows.get_mut(&hwnd) {
            w.erase = true;
        }
    }
    let erased = if update.is_empty() { true } else { send_erase(c, hwnd, hdc)? };
    if ps == 0 {
        release_dc(c, hdc)?;
        return Ok(0);
    }
    let abi = c.p.abi;
    c.g.wptr(ps + off!(abi, paintstruct::HDC), handle_ret(hdc))?;
    c.g.w32(ps + off!(abi, paintstruct::FERASE), (!erased) as u32)?;
    write_rect(&mut c.g, ps + off!(abi, paintstruct::RCPAINT), &update)?;
    Ok(handle_ret(hdc))
}

/// `NtUserEndPaint(hwnd, ps)`.
pub fn nt_user_end_paint(c: &mut Call<'_>) -> R64 {
    let (h, ps) = (c.a32(0), c.a(1));
    let top = {
        let u = c.p.user.lock().unwrap();
        toplevel(&u, window::full(&u, h))
    };
    if let Some(t) = top {
        gdi::surface::flush(c, t);
    }
    if ps == 0 {
        return Ok(0);
    }
    let hdc = c.g.ptr(ps + off!(c.p.abi, paintstruct::HDC))? as u32;
    release_dc(c, hdc)?;
    Ok(1)
}

fn toplevel(u: &User, mut h: u32) -> Option<u32> {
    loop {
        let w = u.windows.get(&h)?;
        if w.style & ws::CHILD == 0 || w.parent == u.desktop {
            return Some(h);
        }
        h = w.parent;
    }
}

/// Read an optional client rectangle argument.
fn opt_rect(c: &Call<'_>, p: u64) -> R<Option<Rect>> {
    Ok(if p == 0 { None } else { Some(read_rect(&c.g, p)?) })
}

/// The windows an hwnd of 0 means for InvalidateRect: every top-level one.
fn targets(c: &Call<'_>, h: u32) -> Vec<u32> {
    let u = c.p.user.lock().unwrap();
    if h == 0 {
        return u.windows.get(&u.desktop).map(|d| d.children.clone()).unwrap_or_default();
    }
    let f = window::full(&u, h);
    if f == 0 {
        Vec::new()
    } else {
        vec![f]
    }
}

/// `NtUserInvalidateRect(hwnd, rect, erase)`.
pub fn nt_user_invalidate_rect(c: &mut Call<'_>) -> R64 {
    let (h, rp, erase) = (c.a32(0), c.a(1), c.a32(2) != 0);
    let r = opt_rect(c, rp)?;
    let ts = targets(c, h);
    if ts.is_empty() && h != 0 {
        set_last_error(c, err::INVALID_WINDOW_HANDLE);
        return Ok(0);
    }
    for w in ts {
        let children = no_clipchildren(c, w);
        invalidate(c, w, r, erase, children);
    }
    Ok(1)
}

/// Whether invalidating `w` reaches its children (RedrawWindow without
/// RDW_ALLCHILDREN or RDW_NOCHILDREN: when it lacks WS_CLIPCHILDREN).
fn no_clipchildren(c: &Call<'_>, w: u32) -> bool {
    c.p.user.lock().unwrap().windows.get(&w).is_some_and(|w| w.style & ws::CLIPCHILDREN == 0)
}

/// `NtUserValidateRect(hwnd, rect)`.
pub fn nt_user_validate_rect(c: &mut Call<'_>) -> R64 {
    let (h, rp) = (c.a32(0), c.a(1));
    let r = opt_rect(c, rp)?;
    for w in targets(c, h) {
        let children = no_clipchildren(c, w);
        validate(c, w, r, children);
    }
    Ok(1)
}

/// A region argument's bounding box, when it is one rectangle (anything
/// more would need the update region to be a region).
fn region_rect(c: &mut Call<'_>, rgn: u32, what: &str) -> R<Option<Rect>> {
    if rgn == 0 {
        return Ok(None);
    }
    match gdi::region::rects(c, rgn) {
        Some(v) if v.len() <= 1 => Ok(Some(v.first().copied().unwrap_or_default())),
        Some(_) => Err(c.unimplemented(&format!("{what} with a complex region"))),
        None => Ok(Some(Rect::default())),
    }
}

/// `NtUserInvalidateRgn(hwnd, rgn, erase)`: over-invalidating the bounding
/// box is safe.
pub fn nt_user_invalidate_rgn(c: &mut Call<'_>) -> R64 {
    let (h, rgn, erase) = (c.a32(0), c.a32(1), c.a32(2) != 0);
    let r = match rgn {
        0 => None,
        _ => gdi::region::rects(c, rgn).map(|v| gdi::region::bounds(&v)),
    };
    for w in targets(c, h) {
        let children = no_clipchildren(c, w);
        invalidate(c, w, r, erase, children);
    }
    Ok(1)
}

/// `NtUserValidateRgn(hwnd, rgn)`.
pub fn nt_user_validate_rgn(c: &mut Call<'_>) -> R64 {
    let (h, rgn) = (c.a32(0), c.a32(1));
    let r = region_rect(c, rgn, "ValidateRgn")?;
    for w in targets(c, h) {
        validate(c, w, r, false);
    }
    Ok(1)
}

/// `NtUserGetUpdateRect(hwnd, rect, erase)`.
pub fn nt_user_get_update_rect(c: &mut Call<'_>) -> R64 {
    let (h, rp, erase) = (c.a32(0), c.a(1), c.a32(2) != 0);
    let hwnd = window::full(&c.p.user.lock().unwrap(), h);
    let Some(update) = c.p.user.lock().unwrap().windows.get(&hwnd).map(|w| w.update) else { return Ok(0) };
    if erase && !update.is_empty() {
        let hdc = get_dc_ex(c, hwnd, 0, dcx::USESTYLE)?;
        with_dc(c, hdc, |d| d.clip = Some(vec![update]));
        send_erase(c, hwnd, hdc)?;
        release_dc(c, hdc)?;
    }
    if rp != 0 {
        write_rect(&mut c.g, rp, &update)?;
    }
    Ok(!update.is_empty() as u64)
}

/// `NtUserGetUpdateRgn(hwnd, rgn, erase)`: the region's kind.
pub fn nt_user_get_update_rgn(c: &mut Call<'_>) -> R64 {
    let (h, rgn, erase) = (c.a32(0), c.a32(1), c.a32(2) != 0);
    let hwnd = window::full(&c.p.user.lock().unwrap(), h);
    let Some(update) = c.p.user.lock().unwrap().windows.get(&hwnd).map(|w| w.update) else { return Ok(0) };
    if erase && !update.is_empty() {
        let hdc = get_dc_ex(c, hwnd, 0, dcx::USESTYLE)?;
        with_dc(c, hdc, |d| d.clip = Some(vec![update]));
        send_erase(c, hwnd, hdc)?;
        release_dc(c, hdc)?;
    }
    let v = if update.is_empty() { Vec::new() } else { vec![update] };
    let k = gdi::region::kind(&v);
    gdi::region::set(c, rgn, v);
    Ok(k)
}

/// Send WM_PAINT now to `hwnd` (and its children with `children`) where
/// something is invalid, as UpdateWindow does.
fn update_now(c: &mut Call<'_>, hwnd: u32, children: bool) -> R<()> {
    let tid = c.t.tid;
    let pending = {
        let u = c.p.user.lock().unwrap();
        u.windows.get(&hwnd).is_some_and(|w| w.tid == tid && w.style & ws::VISIBLE != 0 && (!w.update.is_empty() || w.internal_paint))
    };
    if pending {
        winproc::send_message(c, hwnd, wm::PAINT, 0, 0, Pack::None)?;
    }
    if children {
        let kids = c.p.user.lock().unwrap().windows.get(&hwnd).map(|w| w.children.clone()).unwrap_or_default();
        for k in kids {
            update_now(c, k, true)?;
        }
    }
    Ok(())
}

/// Send any owed WM_ERASEBKGND now.
fn erase_now(c: &mut Call<'_>, hwnd: u32, children: bool) -> R<()> {
    let owed = c.p.user.lock().unwrap().windows.get(&hwnd).map(|w| (w.erase, w.update));
    if let Some((true, update)) = owed {
        let hdc = get_dc_ex(c, hwnd, 0, dcx::USESTYLE)?;
        with_dc(c, hdc, |d| d.clip = Some(vec![update]));
        send_erase(c, hwnd, hdc)?;
        release_dc(c, hdc)?;
    }
    if children {
        let kids = c.p.user.lock().unwrap().windows.get(&hwnd).map(|w| w.children.clone()).unwrap_or_default();
        for k in kids {
            erase_now(c, k, true)?;
        }
    }
    Ok(())
}

/// `NtUserRedrawWindow(hwnd, rect, rgn, flags)`.
pub fn nt_user_redraw_window(c: &mut Call<'_>) -> R64 {
    let (h, rp, rgn, flags) = (c.a32(0), c.a(1), c.a32(2), c.a32(3));
    let hwnd = {
        let u = c.p.user.lock().unwrap();
        if h == 0 {
            u.desktop
        } else {
            window::full(&u, h)
        }
    };
    if hwnd == 0 {
        set_last_error(c, err::INVALID_WINDOW_HANDLE);
        return Ok(0);
    }
    let r = if rp != 0 {
        Some(read_rect(&c.g, rp)?)
    } else if rgn != 0 {
        gdi::region::rects(c, rgn).map(|v| gdi::region::bounds(&v))
    } else {
        None
    };
    let children = if flags & rdw::ALLCHILDREN != 0 {
        true
    } else if flags & rdw::NOCHILDREN != 0 {
        false
    } else {
        no_clipchildren(c, hwnd)
    };
    let desktop = hwnd == c.p.user.lock().unwrap().desktop;
    let wins = if desktop { targets(c, 0) } else { vec![hwnd] };
    for w in &wins {
        let w = *w;
        if flags & rdw::INVALIDATE != 0 {
            invalidate(c, w, r, flags & rdw::ERASE != 0, children || desktop);
            if flags & rdw::INTERNALPAINT != 0 {
                if let Some(win) = c.p.user.lock().unwrap().windows.get_mut(&w) {
                    win.internal_paint = true;
                }
            }
        } else if flags & rdw::VALIDATE != 0 {
            if rp != 0 || rgn != 0 {
                let r = region_rect(c, rgn, "RedrawWindow(RDW_VALIDATE)")?.or(r);
                validate(c, w, r, children);
            } else {
                validate(c, w, None, children);
            }
            if flags & rdw::NOINTERNALPAINT != 0 {
                if let Some(win) = c.p.user.lock().unwrap().windows.get_mut(&w) {
                    win.internal_paint = false;
                }
            }
            if flags & rdw::NOERASE != 0 {
                if let Some(win) = c.p.user.lock().unwrap().windows.get_mut(&w) {
                    win.erase = false;
                }
            }
        }
        if flags & rdw::UPDATENOW != 0 {
            update_now(c, w, children || desktop)?;
        } else if flags & rdw::ERASENOW != 0 {
            erase_now(c, w, children || desktop)?;
        }
    }
    Ok(1)
}

/// `NtUserExcludeUpdateRgn(hdc, hwnd)`: take the window's update region out
/// of the DC's clip region.
pub fn nt_user_exclude_update_rgn(c: &mut Call<'_>) -> R64 {
    let (hdc, h) = (c.a32(0), c.a32(1));
    let hwnd = window::full(&c.p.user.lock().unwrap(), h);
    let Some(update) = c.p.user.lock().unwrap().windows.get(&hwnd).map(|w| w.update) else { return Ok(0) };
    let Some(vis) = gdi::blit::device_area(c, hdc)? else { return Ok(0) };
    let left = gdi::region::coalesce(gdi::region::subtract(&vis, &[update]));
    let k = gdi::region::kind(&left);
    with_dc(c, hdc, |d| d.clip = Some(left));
    Ok(k)
}

/// DefWindowProc's WM_ERASEBKGND: fill the DC's clip box with the class
/// background brush. 1 if erased.
pub fn default_erase(c: &mut Call<'_>, hwnd: u32, hdc: u32) -> R64 {
    let brush = {
        let u = c.p.user.lock().unwrap();
        match u.windows.get(&hwnd) {
            Some(w) => window::class_of(&u, w).background,
            None => return Ok(0),
        }
    };
    if brush == 0 {
        return Ok(0);
    }
    // A system color index + 1 (COLOR_WINDOW + 1 and the like).
    let brush = if brush <= 31 { gdi::sys_color_brush(c, brush as u32 - 1)? } else { brush as u32 };
    let Some(area) = gdi::blit::device_area(c, hdc)? else { return Ok(0) };
    let Some(Obj::Dc(d)) = gdi::get(c, hdc) else { return Ok(0) };
    let (lx, ly) = gdi::blit::logical_offset(c, d.attr)?;
    let b = gdi::region::bounds(&area).offset(-lx, -ly);
    gdi::blit::fill_rect(c, hdc, &b, brush)?;
    Ok(1)
}

/// DefWindowProc's WM_PAINT: validate by painting nothing.
pub fn default_paint(c: &mut Call<'_>, hwnd: u32) -> R64 {
    let (update, _) = {
        let mut u = c.p.user.lock().unwrap();
        let Some(w) = u.windows.get_mut(&hwnd) else { return Ok(0) };
        let r = (w.update, w.erase);
        w.update = Rect::default();
        w.internal_paint = false;
        r
    };
    if !update.is_empty() {
        let hdc = get_dc_ex(c, hwnd, 0, dcx::USESTYLE)?;
        with_dc(c, hdc, |d| d.clip = Some(vec![update]));
        send_erase(c, hwnd, hdc)?;
        release_dc(c, hdc)?;
    }
    Ok(0)
}

/// A window that became visible or changed size gets repainted whole.
pub fn window_exposed(c: &mut Call<'_>, hwnd: u32) {
    invalidate(c, hwnd, None, true, true);
}

/// A window's resources on destruction.
pub fn window_destroyed(c: &mut Call<'_>, surface: u32, own_dc: u32) -> R<()> {
    gdi::surface::release_window(c, surface, own_dc)
}
