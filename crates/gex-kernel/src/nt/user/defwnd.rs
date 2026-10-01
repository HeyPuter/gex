//! `DefWindowProc`: what win32u's `default_window_proc` does for the
//! messages the window manager defines (defwnd.c).
//!
//! Messages with no default action return 0, as on Windows. Messages whose
//! default action gex does not perform yet fail loudly (the non-client mouse
//! handling, the system menu).

use super::window::{self, wm, ws, Rect};
use super::winproc::Pack;
use super::{handle_ret, read_rect, set_last_error, write_rect, R64};
use crate::nt::{Call, R};

/// Windows-1252, the ANSI code page of gex's en-US system, as UTF-16.
const CP1252_HIGH: [u16; 32] = [
    0x20ac, 0x0081, 0x201a, 0x0192, 0x201e, 0x2026, 0x2020, 0x2021, 0x02c6, 0x2030, 0x0160, 0x2039, 0x0152, 0x008d,
    0x017d, 0x008f, 0x0090, 0x2018, 0x2019, 0x201c, 0x201d, 0x2022, 0x2013, 0x2014, 0x02dc, 0x2122, 0x0161, 0x203a,
    0x0153, 0x009d, 0x017e, 0x0178,
];

pub fn ansi_to_w(b: &[u8]) -> Vec<u16> {
    b.iter().map(|&c| if (0x80..0xa0).contains(&c) { CP1252_HIGH[(c - 0x80) as usize] } else { c as u16 }).collect()
}

pub fn w_to_ansi(s: &[u16]) -> Vec<u8> {
    s.iter()
        .map(|&c| {
            if c < 0x80 || (0xa0..0x100).contains(&c) {
                c as u8
            } else {
                CP1252_HIGH.iter().position(|&x| x == c).map(|i| 0x80 + i as u8).unwrap_or(b'?')
            }
        })
        .collect()
}

/// Read a text argument (lParam of WM_SETTEXT, a CREATESTRUCT name).
fn read_text(c: &Call<'_>, p: u64, ansi: bool) -> R<Vec<u16>> {
    if p == 0 {
        return Ok(Vec::new());
    }
    Ok(if ansi { ansi_to_w(&super::read_strz(&c.g, p)?) } else { super::read_wstrz(&c.g, p)? })
}

fn set_text(c: &mut Call<'_>, hwnd: u32, text: Vec<u16>) {
    if let Some(w) = window::find_mut(&mut c.p.user.lock().unwrap(), hwnd) {
        w.text = text;
    }
}

/// `NtUserMessageCall(NtUserDefWindowProc)`.
pub fn default_window_proc(c: &mut Call<'_>, h: u32, msg: u32, wparam: u64, lparam: u64, ansi: bool) -> R64 {
    let hwnd = window::full(&c.p.user.lock().unwrap(), h);
    if hwnd == 0 {
        set_last_error(c, super::err::INVALID_WINDOW_HANDLE);
        return Ok(0);
    }
    match msg {
        wm::NCCREATE => {
            // The window text comes from the CREATESTRUCT's name.
            if lparam != 0 {
                let abi = c.p.abi;
                let name = c.g.ptr(lparam + crate::nt::off!(abi, createstructw::LPSZNAME))?;
                if name >> 16 != 0 {
                    let t = read_text(c, name, ansi)?;
                    set_text(c, hwnd, t);
                }
            }
            Ok(1)
        }
        wm::NCCALCSIZE => {
            let (style, ex, menu) = {
                let u = c.p.user.lock().unwrap();
                let w = window::find(&u, hwnd).unwrap();
                (w.style, w.ex_style, w.menu != 0 && w.style & ws::CHILD == 0)
            };
            // wparam FALSE: a RECT; TRUE: NCCALCSIZE_PARAMS, rgrc[0] first.
            let r = read_rect(&c.g, lparam)?;
            let client = window::nc_calc_client(&r, style, ex, menu);
            write_rect(&mut c.g, lparam, &client)?;
            Ok(0)
        }
        wm::SETTEXT => {
            let t = read_text(c, lparam, ansi)?;
            set_text(c, hwnd, t);
            Ok(1)
        }
        wm::GETTEXT => {
            let text = window::find(&c.p.user.lock().unwrap(), hwnd).map(|w| w.text.clone()).unwrap_or_default();
            let cap = wparam as usize;
            if cap == 0 || lparam == 0 {
                return Ok(0);
            }
            if ansi {
                let a = w_to_ansi(&text);
                let n = a.len().min(cap - 1);
                let mut z = a[..n].to_vec();
                z.push(0);
                c.g.write(lparam, &z)?;
                Ok(n as u64)
            } else {
                let n = text.len().min(cap - 1);
                let mut z = text[..n].to_vec();
                z.push(0);
                c.g.write_wchars(lparam, &z)?;
                Ok(n as u64)
            }
        }
        wm::GETTEXTLENGTH => {
            let text = window::find(&c.p.user.lock().unwrap(), hwnd).map(|w| w.text.clone()).unwrap_or_default();
            Ok(if ansi { w_to_ansi(&text).len() } else { text.len() } as u64)
        }
        wm::CLOSE => {
            window::destroy_full(c, hwnd)?;
            Ok(0)
        }
        wm::WINDOWPOSCHANGED => {
            // WM_MOVE and WM_SIZE for what changed (`handle_window_pos_changed`).
            let abi = c.p.abi;
            let flags = c.g.u32(lparam + crate::nt::off!(abi, windowpos::FLAGS))?;
            const SWP_NOCLIENTSIZE: u32 = 0x0800;
            const SWP_NOCLIENTMOVE: u32 = 0x1000;
            let cr = window::find(&c.p.user.lock().unwrap(), hwnd).map(|w| w.client_rect).unwrap_or_default();
            if flags & SWP_NOCLIENTMOVE == 0 {
                let pos = (cr.left as u32 as u64 & 0xffff) | ((cr.top as u32 as u64 & 0xffff) << 16);
                super::winproc::send_message(c, hwnd, wm::MOVE, 0, pos, Pack::None)?;
            }
            if flags & SWP_NOCLIENTSIZE == 0 {
                let size = (cr.width() as u32 as u64 & 0xffff) | ((cr.height() as u32 as u64 & 0xffff) << 16);
                super::winproc::send_message(c, hwnd, wm::SIZE, 0, size, Pack::None)?;
            }
            Ok(0)
        }
        wm::ACTIVATE => {
            // An activated window takes the focus.
            if wparam & 0xffff != 0 {
                let style = window::find(&c.p.user.lock().unwrap(), hwnd).map(|w| w.style).unwrap_or(0);
                if style & ws::MINIMIZE == 0 {
                    window::set_focus(c, hwnd)?;
                }
            }
            Ok(0)
        }
        wm::NCACTIVATE => Ok(1),
        wm::MOUSEACTIVATE => Ok(1), // MA_ACTIVATE
        wm::NCHITTEST => {
            // HTCLIENT inside the client area, HTCAPTION above it, HTBORDER
            // around it (the non-client layout is Wine's).
            let (x, y) = ((lparam as u32 & 0xffff) as i16 as i32, (lparam as u32 >> 16) as i16 as i32);
            let u = c.p.user.lock().unwrap();
            let w = window::find(&u, hwnd).unwrap();
            let wr = window::screen_window_rect(&u, w);
            let cr = window::screen_client_rect(&u, w);
            let inside = |r: &Rect| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
            Ok(if !inside(&wr) {
                0 // HTNOWHERE
            } else if inside(&cr) {
                1 // HTCLIENT
            } else if y < cr.top && w.style & ws::CAPTION == ws::CAPTION {
                2 // HTCAPTION
            } else {
                18 // HTBORDER
            } as u64)
        }
        wm::SHOWWINDOW | wm::SETFOCUS | wm::KILLFOCUS | wm::ACTIVATEAPP | wm::CREATE | wm::DESTROY
        | wm::NCDESTROY | wm::MOVE | wm::SIZE | wm::GETMINMAXINFO | wm::WINDOWPOSCHANGING | wm::STYLECHANGING
        | wm::STYLECHANGED | wm::ENABLE | wm::PARENTNOTIFY | wm::TIMER | wm::SYSTIMER | wm::SETCURSOR => Ok(0),
        // Keyboard and character messages have no default action (the Alt
        // and F10 menu keys excepted, below).
        0x100 | 0x101 | 0x102 | 0x103 | 0x109 => Ok(0),
        0x02b1 => Ok(0), // WM_WTSSESSION_CHANGE
        0x0011 => Ok(1), // WM_QUERYENDSESSION
        0x001f => Ok(0), // WM_CANCELMODE
        0x007f => Ok(0), // WM_GETICON: no icons
        0x0080 => Ok(0), // WM_SETICON: the previous icon, none (the frame draws none)
        0x0055 => Ok(2), // WM_NOTIFYFORMAT: NFR_UNICODE
        0x0281..=0x0291 => Ok(0), // WM_IME_*
        0x0200..=0x020e => Ok(0), // client-area mouse messages
        0x02a1..=0x02a3 => Ok(0), // WM_MOUSEHOVER, WM_NCMOUSELEAVE, WM_MOUSELEAVE
        wm::ERASEBKGND => super::paint::default_erase(c, hwnd, wparam as u32),
        wm::PAINT => super::paint::default_paint(c, hwnd),
        // The compositor draws window frames (§6a.1).
        wm::NCPAINT => Ok(0),
        0x0112 => {
            // WM_SYSCOMMAND: SC_CLOSE sends WM_CLOSE.
            if wparam & 0xfff0 == 0xf060 {
                super::winproc::send_message(c, hwnd, wm::CLOSE, 0, 0, Pack::None)?;
                Ok(0)
            } else {
                Err(c.unimplemented(&format!("DefWindowProc WM_SYSCOMMAND {:#x}", wparam & 0xfff0)))
            }
        }
        m if m >= wm::USER => Ok(0),
        m => Err(c.unimplemented(&format!("DefWindowProc message {m:#x}"))),
    }
}

#[allow(dead_code)]
fn _unused() -> u64 {
    handle_ret(0)
}
