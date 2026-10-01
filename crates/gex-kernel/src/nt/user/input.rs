//! Input: the keyboard (Windows' US layout, as win32u has it), the mouse and
//! cursor, cursor and icon objects, and hardware messages (win32u's input.c,
//! cursoricon.c, and wineserver's queue.c for the hardware queue).
//!
//! Hardware input arrives through [`inject_key`] and [`inject_mouse`], which
//! update the asynchronous key state and queue hardware messages to the
//! thread that owns the focus (keys) or the window under the cursor (mouse),
//! as wineserver does. A thread's synchronous key state (GetKeyState) follows
//! the messages it retrieves. The display side's input ring will feed the
//! same two functions; until it exists, `GEX_NT_INPUT` scripts input by frame
//! number (see [`script_tick`]), which also serves deterministic tests.

use super::kbdus;
use super::message::{self, qs, Msg};
use super::window::{self, ws, Rect};
use super::{err, handle_ret, read_rect, set_last_error, write_rect, ObjKind, User, R64};
use crate::nt::{Call, R};
use std::collections::HashMap;

pub mod vk {
    pub const LBUTTON: u8 = 0x01;
    pub const RBUTTON: u8 = 0x02;
    pub const MBUTTON: u8 = 0x04;
    pub const SHIFT: u8 = 0x10;
    pub const CONTROL: u8 = 0x11;
    pub const MENU: u8 = 0x12;
    pub const CAPITAL: u8 = 0x14;
    pub const F10: u8 = 0x79;
    pub const LSHIFT: u8 = 0xa0;
    pub const RSHIFT: u8 = 0xa1;
    pub const LCONTROL: u8 = 0xa2;
    pub const RCONTROL: u8 = 0xa3;
    pub const LMENU: u8 = 0xa4;
    pub const RMENU: u8 = 0xa5;
}

/// The US layout's HKL.
pub const HKL_US: u64 = 0x0409_0409;

/// A cursor or icon object (Wine's `struct cursoricon_object`, without the
/// pixels: those stay in the GDI bitmaps the description names).
#[derive(Clone, Debug, Default)]
pub struct CursorIcon {
    pub is_icon: bool,
    pub module: Vec<u16>,
    /// A resource name, or `#id` for an integer resource.
    pub res_name: Vec<u16>,
    pub width: u32,
    pub height: u32,
    pub color: u32,
    pub mask: u32,
    pub alpha: u32,
    pub hotspot: (i32, i32),
    pub has_data: bool,
    /// The HRSRC it was loaded from: what user32's shared-icon cache looks
    /// up (with the module), as in Wine.
    pub rsrc: u64,
}

#[derive(Default, Debug)]
pub struct InputState {
    /// Asynchronous key state: 0x80 down, 0x01 toggled, 0x40 pressed since
    /// GetAsyncKeyState last looked.
    pub keys: super::KeyState,
    pub cursor: (i32, i32),
    pub clip: Option<Rect>,
    pub cursor_handle: u32,
    pub show_count: i32,
    pub icons: HashMap<u32, CursorIcon>,
    /// Scripted input still to run: (frame, action).
    pub script: Option<Vec<(u64, Action)>>,
    /// TrackMouseEvent(TME_LEAVE): the window to tell when the mouse leaves.
    pub track_leave: u32,
}

/// One scripted input event, in client coordinates of the window whose
/// frame triggers it.
#[derive(Clone, Copy, Debug)]
pub enum Action {
    Move(i32, i32),
    Down(i32, i32, u8),
    Up(i32, i32, u8),
    KeyDown(u8),
    KeyUp(u8),
}

/// A thread's hardware message, with what retrieving it does to the
/// thread's key state.
#[derive(Clone, Copy, Debug)]
pub struct HwMsg {
    pub msg: Msg,
    pub vk: u8,
    pub down: bool,
}

// ------------------------------------------------------------ the keyboard

/// The generic key for a left/right one (wParam of WM_KEYDOWN).
fn generic(v: u8) -> u8 {
    match v {
        vk::LSHIFT | vk::RSHIFT => vk::SHIFT,
        vk::LCONTROL | vk::RCONTROL => vk::CONTROL,
        vk::LMENU | vk::RMENU => vk::MENU,
        v => v,
    }
}

/// The scan code of a virtual key, with 0xe000 for extended ones.
pub fn vk_to_scan(v: u8) -> u32 {
    let v = match v {
        vk::SHIFT => vk::LSHIFT,
        vk::CONTROL => vk::LCONTROL,
        vk::MENU => vk::LMENU,
        v => v,
    };
    match kbdus::VSC2VK.iter().position(|&x| (x & 0xff) as u8 == v) {
        Some(i) if i >= 0x100 => (i as u32 - 0x100) | 0xe000,
        Some(i) => i as u32,
        None => 0,
    }
}

/// Update a key state table for `v` going down or up.
fn set_key(keys: &mut [u8; 256], v: u8, down: bool) {
    let mut one = |k: u8| {
        let k = k as usize;
        if down {
            if keys[k] & 0x80 == 0 {
                keys[k] ^= 0x01;
            }
            keys[k] |= 0x80;
        } else {
            keys[k] &= !0x80;
        }
    };
    one(v);
    let g = generic(v);
    if g != v {
        one(g);
    }
}

/// `NtUserGetKeyboardLayout(tid)`.
pub fn nt_user_get_keyboard_layout(_c: &mut Call<'_>) -> R64 {
    Ok(HKL_US)
}

/// `NtUserGetKeyboardLayoutList(size, layouts)`.
pub fn nt_user_get_keyboard_layout_list(c: &mut Call<'_>) -> R64 {
    let (size, out) = (c.a32(0) as i32, c.a(1));
    if size > 0 && out != 0 {
        c.g.wptr(out, HKL_US)?;
    }
    Ok(1)
}

/// `NtUserActivateKeyboardLayout(layout, flags)`: the US layout only.
pub fn nt_user_activate_keyboard_layout(c: &mut Call<'_>) -> R64 {
    let layout = c.a(0);
    if layout != HKL_US && layout != 1 && layout != 0 {
        return Err(c.unimplemented(&format!("keyboard layout {layout:#x}")));
    }
    Ok(HKL_US)
}

/// `NtUserMapVirtualKeyEx(code, type, layout)` (Wine's, on kbdus).
pub fn nt_user_map_virtual_key_ex(c: &mut Call<'_>) -> R64 {
    let (code, ty) = (c.a32(0), c.a32(1));
    let vsc2vk = &kbdus::VSC2VK;
    let ret: u32 = match ty {
        // MAPVK_VK_TO_VSC, MAPVK_VK_TO_VSC_EX
        0 | 4 => {
            let code = match code as u8 {
                vk::SHIFT => vk::LSHIFT as u32,
                vk::CONTROL => vk::LCONTROL as u32,
                vk::MENU => vk::LMENU as u32,
                0x60 => 0x2d, // numpad 0..9 and decimal: their navigation keys
                0x61 => 0x23,
                0x62 => 0x28,
                0x63 => 0x22,
                0x64 => 0x25,
                0x65 => 0x0c,
                0x66 => 0x27,
                0x67 => 0x24,
                0x68 => 0x26,
                0x69 => 0x21,
                0x6e => 0x2e,
                _ => code,
            };
            let mut r = vsc2vk.iter().position(|&x| (x & 0xff) as u32 == code).unwrap_or(0) as u32;
            if ty == 0 {
                r = if r >= 0x200 { 0 } else { r & 0xff };
            } else if r >= 0x100 {
                r += 0xdf00;
            }
            r
        }
        // MAPVK_VSC_TO_VK, MAPVK_VSC_TO_VK_EX
        1 | 3 => {
            let code = if code & 0xe000 != 0 { code.wrapping_sub(0xdf00) } else { code };
            let r = vsc2vk.get(code as usize).map(|&x| (x & 0xff) as u32).unwrap_or(0);
            if ty == 1 {
                generic(r as u8) as u32
            } else {
                r
            }
        }
        // MAPVK_VK_TO_CHAR
        2 => {
            if (b'A' as u32..=b'Z' as u32).contains(&code) {
                code
            } else {
                kbdus::VK2CHAR.get(code as usize).copied().unwrap_or(0) as u32
            }
        }
        _ => return Err(c.unimplemented(&format!("MapVirtualKey type {ty}"))),
    };
    Ok(ret as u64)
}

/// `NtUserGetKeyState(vk)`: the calling thread's synchronous state.
pub fn nt_user_get_key_state(c: &mut Call<'_>) -> R64 {
    let v = c.a32(0) as usize & 0xff;
    let tid = c.t.tid;
    let s = c.p.user.lock().unwrap().thread(tid).keys[v];
    let mut r: u16 = 0;
    if s & 0x80 != 0 {
        r |= 0xff80;
    }
    if s & 0x01 != 0 {
        r |= 0x0001;
    }
    Ok(r as i16 as i64 as u64)
}

/// `NtUserGetAsyncKeyState(vk)`.
pub fn nt_user_get_async_key_state(c: &mut Call<'_>) -> R64 {
    let v = c.a32(0) as usize & 0xff;
    let mut u = c.p.user.lock().unwrap();
    let s = &mut u.input.keys[v];
    let mut r: u16 = 0;
    if *s & 0x80 != 0 {
        r |= 0x8000;
    }
    if *s & 0x40 != 0 {
        r |= 0x0001;
        *s &= !0x40;
    }
    Ok(r as i16 as i64 as u64)
}

/// `NtUserGetKeyboardState(state)`.
pub fn nt_user_get_keyboard_state(c: &mut Call<'_>) -> R64 {
    let out = c.a(0);
    let tid = c.t.tid;
    let keys = c.p.user.lock().unwrap().thread(tid).keys;
    let v: Vec<u8> = keys.iter().map(|&s| s & 0x81).collect();
    c.g.write(out, &v)?;
    Ok(1)
}

/// `NtUserSetKeyboardState(state)`.
pub fn nt_user_set_keyboard_state(c: &mut Call<'_>) -> R64 {
    let p = c.a(0);
    let b = c.g.bytes(p, 256)?;
    let tid = c.t.tid;
    let mut u = c.p.user.lock().unwrap();
    let t = u.thread(tid);
    for (k, v) in t.keys.iter_mut().zip(b) {
        *k = v & 0x81;
    }
    Ok(1)
}

/// The character a key makes under `keys` (a key state table), as
/// ToUnicode would: `None` for a key that makes none.
fn key_char(v: u8, keys: &[u8; 256]) -> Option<u16> {
    let shift = keys[vk::SHIFT as usize] & 0x80 != 0;
    let ctrl = keys[vk::CONTROL as usize] & 0x80 != 0;
    let alt = keys[vk::MENU as usize] & 0x80 != 0;
    let caps = keys[vk::CAPITAL as usize] & 0x01 != 0;
    if ctrl && alt {
        return None;
    }
    if v.is_ascii_uppercase() {
        if ctrl {
            return Some((v - b'A' + 1) as u16);
        }
        let upper = shift ^ caps;
        return Some(if upper { v as u16 } else { v.to_ascii_lowercase() as u16 });
    }
    if ctrl {
        // Ctrl+[ \ ] and the like; nothing else makes a character.
        return match v {
            0xdb => Some(0x1b),
            0xdc => Some(0x1c),
            0xdd => Some(0x1d),
            0x08 => Some(0x7f),
            0x0d => Some(0x0a),
            _ => None,
        };
    }
    let ch = if shift { kbdus::VK2SHIFTED[v as usize] } else { kbdus::VK2CHAR[v as usize] as u16 };
    (ch != 0).then_some(ch)
}

/// `NtUserToUnicodeEx(vk, scan, state, buf, size, flags, layout)`.
pub fn nt_user_to_unicode_ex(c: &mut Call<'_>) -> R64 {
    let (v, scan, state, buf, size) = (c.a32(0), c.a32(1), c.a(2), c.a(3), c.a32(4) as i32);
    if scan & 0x8000 != 0 {
        return Ok(0); // a key release
    }
    let mut keys = [0u8; 256];
    if state != 0 {
        keys.copy_from_slice(&c.g.bytes(state, 256)?);
    }
    match key_char(v as u8, &keys) {
        Some(ch) if size > 0 && buf != 0 => {
            c.g.w16(buf, ch)?;
            if size > 1 {
                c.g.w16(buf + 2, 0)?;
            }
            Ok(1)
        }
        _ => Ok(0),
    }
}

/// `NtUserVkKeyScanEx(ch, layout)`: the key and shift state for a character.
pub fn nt_user_vk_key_scan_ex(c: &mut Call<'_>) -> R64 {
    let ch = c.a32(0) as u16;
    if (b'a' as u16..=b'z' as u16).contains(&ch) {
        return Ok((ch - 0x20) as u64);
    }
    if (b'A' as u16..=b'Z' as u16).contains(&ch) {
        return Ok(ch as u64 | 0x100);
    }
    for v in 0..256usize {
        if kbdus::VK2CHAR[v] as u16 == ch && ch != 0 {
            return Ok(v as u64);
        }
        if kbdus::VK2SHIFTED[v] == ch && ch != 0 {
            return Ok(v as u64 | 0x100);
        }
    }
    Ok(0xffff_ffff_ffff_ffff)
}

/// TranslateMessage: WM_CHAR (or WM_SYSCHAR) for a key down that makes a
/// character, posted to the same window.
pub fn translate(c: &mut Call<'_>, m: &Msg) -> R<bool> {
    const WM_KEYDOWN: u32 = 0x100;
    const WM_SYSKEYDOWN: u32 = 0x104;
    if m.msg != WM_KEYDOWN && m.msg != WM_SYSKEYDOWN {
        return Ok(false);
    }
    let tid = c.t.tid;
    let keys = c.p.user.lock().unwrap().thread(tid).keys;
    let Some(ch) = key_char(m.wparam as u8, &keys) else { return Ok(false) };
    let msg = if m.msg == WM_KEYDOWN { 0x102 } else { 0x106 }; // WM_CHAR, WM_SYSCHAR
    message::post_to_thread(c, tid, Msg { msg, wparam: ch as u64, ..*m });
    Ok(true)
}

// ------------------------------------------------------------ hardware input

/// Which thread and window keyboard input goes to: the focus of the
/// foreground window's thread (or its active window).
fn key_target(u: &mut User) -> Option<(u32, u32)> {
    let fg = u.foreground;
    let tid = u.windows.get(&fg)?.tid;
    let t = u.thread(tid);
    let h = if t.focus != 0 { t.focus } else { t.active };
    (h != 0).then_some((tid, h))
}

/// The deepest visible, enabled window at screen point `pt`, top-level
/// windows in z order first.
pub fn window_from_point(u: &User, pt: (i32, i32)) -> u32 {
    fn inside(r: &Rect, pt: (i32, i32)) -> bool {
        pt.0 >= r.left && pt.0 < r.right && pt.1 >= r.top && pt.1 < r.bottom
    }
    fn descend(u: &User, h: u32, pt: (i32, i32)) -> u32 {
        let Some(w) = u.windows.get(&h) else { return h };
        if !inside(&window::screen_client_rect(u, w), pt) {
            return h;
        }
        for &k in &w.children {
            let Some(cw) = u.windows.get(&k) else { continue };
            if cw.style & ws::VISIBLE != 0 && cw.style & ws::DISABLED == 0 && inside(&window::screen_window_rect(u, cw), pt) {
                return descend(u, k, pt);
            }
        }
        h
    }
    let Some(d) = u.windows.get(&u.desktop) else { return 0 };
    for &h in &d.children {
        let Some(w) = u.windows.get(&h) else { continue };
        if w.style & ws::VISIBLE != 0 && inside(&window::screen_window_rect(u, w), pt) {
            return descend(u, h, pt);
        }
    }
    0
}

fn queue_hw(c: &mut Call<'_>, tid: u32, hw: HwMsg, bits: u32) {
    {
        let mut u = c.p.user.lock().unwrap();
        let q = &mut u.thread(tid).queue;
        // Consecutive mouse moves to one window collapse into the last.
        if hw.msg.msg == 0x200 {
            if let Some(last) = q.input.back_mut() {
                if last.msg.msg == 0x200 && last.msg.hwnd == hw.msg.hwnd {
                    *last = hw;
                    return;
                }
            }
        }
        q.input.push_back(hw);
        q.changed |= bits;
    }
    if tid != c.t.tid {
        message::wake_thread(c, tid);
    }
}

/// A key going down or up (a hardware event), with its scan code (`None`:
/// the layout's for the key).
pub fn inject_key(c: &mut Call<'_>, v: u8, down: bool, scan: Option<u32>) {
    let scan = scan.unwrap_or_else(|| vk_to_scan(v));
    let time = message::tick_count(c);
    let (target, sys, alt_held, prev, pt) = {
        let mut u = c.p.user.lock().unwrap();
        let prev = u.input.keys[v as usize] & 0x80 != 0;
        set_key(&mut u.input.keys, v, down);
        if down {
            u.input.keys[v as usize] |= 0x40;
        }
        let alt = u.input.keys[vk::MENU as usize] & 0x80 != 0;
        let ctrl = u.input.keys[vk::CONTROL as usize] & 0x80 != 0;
        let pt = u.input.cursor;
        let sys = (alt && !ctrl) || v == vk::F10 || generic(v) == vk::MENU;
        (key_target(&mut u), sys, alt, prev, pt)
    };
    let Some((tid, hwnd)) = target else { return };
    let msg = match (down, sys) {
        (true, false) => 0x100,
        (false, false) => 0x101,
        (true, true) => 0x104,
        (false, true) => 0x105,
    };
    let mut lparam = 1 | (scan & 0xff) << 16;
    if scan & 0xe000 != 0 {
        lparam |= 1 << 24;
    }
    if alt_held {
        lparam |= 1 << 29;
    }
    if prev {
        lparam |= 1 << 30;
    }
    if !down {
        lparam |= 1 << 31;
    }
    let m = Msg { hwnd, msg, wparam: generic(v) as u64, lparam: lparam as u64, time, pt };
    queue_hw(c, tid, HwMsg { msg: m, vk: v, down }, qs::KEY);
}

/// What a mouse event is.
#[derive(Clone, Copy, Debug)]
pub enum Mouse {
    Move,
    Button(u8, bool),
    Wheel(i32),
}

/// The mouse at screen point `pt` (a hardware event).
pub fn inject_mouse(c: &mut Call<'_>, pt: (i32, i32), ev: Mouse) {
    let time = message::tick_count(c);
    let mut left = None;
    let target = 'target: {
        let mut u = c.p.user.lock().unwrap();
        let pt = match u.input.clip {
            Some(r) => (pt.0.clamp(r.left, r.right - 1), pt.1.clamp(r.top, r.bottom - 1)),
            None => pt,
        };
        u.input.cursor = pt;
        if let Mouse::Button(b, down) = ev {
            set_key(&mut u.input.keys, b, down);
            if down {
                u.input.keys[b as usize] |= 0x40;
            }
        }
        let capture = u.threads.values().map(|t| t.capture).find(|&h| h != 0);
        let hwnd = capture.unwrap_or_else(|| window_from_point(&u, pt));
        // A tracked window the mouse has left hears about it first.
        let t = u.input.track_leave;
        if t != 0 && t != hwnd {
            u.input.track_leave = 0;
            if let Some(tw) = u.windows.get(&t) {
                left = Some((tw.tid, t));
            }
        }
        let Some(w) = u.windows.get(&hwnd) else { break 'target None };
        let cr = window::screen_client_rect(&u, w);
        if capture.is_none() && !(pt.0 >= cr.left && pt.0 < cr.right && pt.1 >= cr.top && pt.1 < cr.bottom) {
            // The non-client area: the compositor's frame, not the program's.
            break 'target None;
        }
        let k = &u.input.keys;
        let mut mk = 0u32;
        for (bit, key) in [(1, vk::LBUTTON), (2, vk::RBUTTON), (4, vk::SHIFT), (8, vk::CONTROL), (0x10, vk::MBUTTON)] {
            if k[key as usize] & 0x80 != 0 {
                mk |= bit;
            }
        }
        Some((w.tid, hwnd, (pt.0 - cr.left, pt.1 - cr.top), mk))
    };
    if let Some((ltid, lh)) = left {
        message::post_to_thread(c, ltid, Msg { hwnd: lh, msg: 0x2a3, wparam: 0, lparam: 0, time, pt: (0, 0) }); // WM_MOUSELEAVE
    }
    let Some((tid, hwnd, client, mk)) = target else { return };
    let pos = |x: i32, y: i32| (x as u16 as u64) | ((y as u16 as u64) << 16);
    let pt_screen = c.p.user.lock().unwrap().input.cursor;
    let (msg, wparam, lparam, vk_, down, bits) = match ev {
        Mouse::Move => (0x200, mk as u64, pos(client.0, client.1), 0, false, qs::MOUSEMOVE),
        Mouse::Button(b, down) => {
            let base = match b {
                vk::LBUTTON => 0x201,
                vk::RBUTTON => 0x204,
                _ => 0x207,
            };
            (base + (!down) as u32, mk as u64, pos(client.0, client.1), b, down, qs::MOUSEBUTTON)
        }
        Mouse::Wheel(d) => (0x20a, (mk as u64) | ((d as u16 as u64) << 16), pos(pt_screen.0, pt_screen.1), 0, false, qs::MOUSEBUTTON),
    };
    let m = Msg { hwnd, msg, wparam, lparam, time, pt: pt_screen };
    queue_hw(c, tid, HwMsg { msg: m, vk: vk_, down }, bits);
}

/// A retrieved hardware message updates the thread's key state.
pub fn retrieved(u: &mut User, tid: u32, hw: &HwMsg) {
    if hw.vk != 0 {
        let t = u.thread(tid);
        set_key(&mut t.keys, hw.vk, hw.down);
    }
}

// ------------------------------------------------------------ scripted input

/// Parse `GEX_NT_INPUT`: `frame:action[;frame:action...]`, with actions
/// `click:x,y` (a left press and, two frames later, its release),
/// `rclick:x,y`, `move:x,y`, `down:x,y`, `up:x,y`, `key:VK` (press and
/// release two frames later; VK in hex or decimal), `keydown:VK`, `keyup:VK`.
/// Coordinates are client coordinates of the window that swaps the frame.
fn parse_script(s: &str) -> Vec<(u64, Action)> {
    let num = |t: &str| -> Option<i64> {
        let t = t.trim();
        match t.strip_prefix("0x") {
            Some(h) => i64::from_str_radix(h, 16).ok(),
            None => t.parse().ok(),
        }
    };
    let mut out = Vec::new();
    for item in s.split(';').filter(|x| !x.trim().is_empty()) {
        let mut parts = item.splitn(3, ':');
        let (Some(f), Some(a)) = (parts.next().and_then(num), parts.next()) else {
            crate::nt_log!("gex-nt: GEX_NT_INPUT: cannot read {item:?}");
            continue;
        };
        let arg = parts.next().unwrap_or("");
        let xy = || {
            let mut it = arg.split(',').map(num);
            Some((it.next()?? as i32, it.next()?? as i32))
        };
        let f = f as u64;
        match (a, xy(), num(arg)) {
            ("click", Some((x, y)), _) => {
                out.push((f, Action::Down(x, y, vk::LBUTTON)));
                out.push((f + 2, Action::Up(x, y, vk::LBUTTON)));
            }
            ("rclick", Some((x, y)), _) => {
                out.push((f, Action::Down(x, y, vk::RBUTTON)));
                out.push((f + 2, Action::Up(x, y, vk::RBUTTON)));
            }
            ("move", Some((x, y)), _) => out.push((f, Action::Move(x, y))),
            ("down", Some((x, y)), _) => out.push((f, Action::Down(x, y, vk::LBUTTON))),
            ("up", Some((x, y)), _) => out.push((f, Action::Up(x, y, vk::LBUTTON))),
            ("key", _, Some(k)) => {
                out.push((f, Action::KeyDown(k as u8)));
                out.push((f + 2, Action::KeyUp(k as u8)));
            }
            ("keydown", _, Some(k)) => out.push((f, Action::KeyDown(k as u8))),
            ("keyup", _, Some(k)) => out.push((f, Action::KeyUp(k as u8))),
            _ => crate::nt_log!("gex-nt: GEX_NT_INPUT: cannot read {item:?}"),
        }
    }
    out
}

/// Run the scripted input due at `frame`, for the window `hwnd` that is
/// presenting it.
pub fn script_tick(c: &mut Call<'_>, frame: u64, hwnd: u32) {
    let due: Vec<Action> = {
        let mut u = c.p.user.lock().unwrap();
        let script = u.input.script.get_or_insert_with(|| {
            std::env::var("GEX_NT_INPUT").map(|s| parse_script(&s)).unwrap_or_default()
        });
        let due = script.iter().filter(|(f, _)| *f == frame).map(|(_, a)| *a).collect();
        script.retain(|(f, _)| *f > frame);
        due
    };
    if due.is_empty() {
        return;
    }
    let origin = {
        let u = c.p.user.lock().unwrap();
        u.windows.get(&hwnd).map(|w| window::screen_client_rect(&u, w)).map(|r| (r.left, r.top)).unwrap_or((0, 0))
    };
    for a in due {
        let at = |x: i32, y: i32| (origin.0 + x, origin.1 + y);
        match a {
            Action::Move(x, y) => inject_mouse(c, at(x, y), Mouse::Move),
            Action::Down(x, y, b) => {
                inject_mouse(c, at(x, y), Mouse::Move);
                inject_mouse(c, at(x, y), Mouse::Button(b, true));
            }
            Action::Up(x, y, b) => inject_mouse(c, at(x, y), Mouse::Button(b, false)),
            Action::KeyDown(k) => inject_key(c, k, true, None),
            Action::KeyUp(k) => inject_key(c, k, false, None),
        }
    }
}

// ------------------------------------------------------------ the cursor

/// `NtUserGetCursorPos(pt)`.
pub fn nt_user_get_cursor_pos(c: &mut Call<'_>) -> R64 {
    let p = c.a(0);
    let (x, y) = c.p.user.lock().unwrap().input.cursor;
    c.g.w32(p, x as u32)?;
    c.g.w32(p + 4, y as u32)?;
    Ok(1)
}

/// `NtUserSetCursorPos(x, y)`: moves the pointer, which (as on Windows)
/// makes a mouse move.
pub fn nt_user_set_cursor_pos(c: &mut Call<'_>) -> R64 {
    let (x, y) = (c.a32(0) as i32, c.a32(1) as i32);
    inject_mouse(c, (x, y), Mouse::Move);
    Ok(1)
}

/// `NtUserClipCursor(rect)`.
pub fn nt_user_clip_cursor(c: &mut Call<'_>) -> R64 {
    let p = c.a(0);
    let r = if p == 0 { None } else { Some(read_rect(&c.g, p)?) };
    let mut u = c.p.user.lock().unwrap();
    u.input.clip = r.filter(|r| !r.is_empty());
    Ok(1)
}

/// `NtUserGetClipCursor(rect)`.
pub fn nt_user_get_clip_cursor(c: &mut Call<'_>) -> R64 {
    let p = c.a(0);
    let r = c.p.user.lock().unwrap().input.clip.unwrap_or(super::sysparams::primary_rect(c));
    write_rect(&mut c.g, p, &r)?;
    Ok(1)
}

/// `NtUserShowCursor(show)`: the display count.
pub fn nt_user_show_cursor(c: &mut Call<'_>) -> R64 {
    let show = c.a32(0) != 0;
    let mut u = c.p.user.lock().unwrap();
    u.input.show_count += if show { 1 } else { -1 };
    Ok(u.input.show_count as i64 as u64)
}

/// `NtUserSetCursor(cursor)`: the previous one.
pub fn nt_user_set_cursor(c: &mut Call<'_>) -> R64 {
    let h = c.a32(0);
    let mut u = c.p.user.lock().unwrap();
    let full = if h == 0 { 0 } else { u.full_handle(h, ObjKind::Icon).unwrap_or(0) };
    Ok(handle_ret(std::mem::replace(&mut u.input.cursor_handle, full)))
}

/// `NtUserGetCursor()` (GetCursor).
pub fn nt_user_get_cursor(c: &mut Call<'_>) -> R64 {
    Ok(handle_ret(c.p.user.lock().unwrap().input.cursor_handle))
}

/// `NtUserGetCursorInfo(info)`: CURSORINFO { cbSize, flags, hCursor, pt }.
pub fn nt_user_get_cursor_info(c: &mut Call<'_>) -> R64 {
    let p = c.a(0);
    let (count, handle, (x, y)) = {
        let u = c.p.user.lock().unwrap();
        (u.input.show_count, u.input.cursor_handle, u.input.cursor)
    };
    let ps = c.p.abi.ptr_size();
    c.g.w32(p + 4, (count >= 0) as u32)?; // CURSOR_SHOWING
    c.g.wptr(p + 8, handle_ret(handle))?;
    c.g.w32(p + 8 + ps, x as u32)?;
    c.g.w32(p + 12 + ps, y as u32)?;
    Ok(1)
}

// ------------------------------------------------------------ cursor objects

fn us_name(c: &Call<'_>, p: u64) -> R<Vec<u16>> {
    if p == 0 {
        return Ok(Vec::new());
    }
    let len = c.g.u16(p)?;
    let buf = c.g.ptr(p + c.p.abi.ptr_size())?;
    if len == 0 {
        // MAKEINTRESOURCE: the id in the buffer pointer.
        return Ok(crate::nt::to_w(&format!("#{buf}")));
    }
    c.g.wchars(buf, len as usize / 2)
}

/// `NtUserCallOneParam(CreateCursorIcon, is_icon)`: an empty object.
pub fn create_cursor_icon(c: &mut Call<'_>, is_icon: bool) -> R64 {
    let mut u = c.p.user.lock().unwrap();
    let Some(h) = u.alloc_handle(ObjKind::Icon) else { return Ok(0) };
    u.input.icons.insert(h, CursorIcon { is_icon, ..Default::default() });
    Ok(handle_ret(h))
}

/// `NtUserSetCursorIconData(cursor, module, res_name, desc)`.
pub fn nt_user_set_cursor_icon_data(c: &mut Call<'_>) -> R64 {
    let (h, module, res, desc) = (c.a32(0), c.a(1), c.a(2), c.a(3));
    let module = us_name(c, module)?;
    let res_name = us_name(c, res)?;
    let ps = c.p.abi.ptr_size();
    // struct cursoricon_desc { flags, num_steps, num_frames, delay, frames,
    // frame_seq, frame_rates, rsrc }
    let frames = c.g.ptr(desc + 16)?;
    let rsrc = c.g.ptr(desc + 16 + 3 * ps)?;
    let (width, height) = (c.g.u32(frames)?, c.g.u32(frames + 4)?);
    let color = c.g.ptr(frames + 8)? as u32;
    let alpha = c.g.ptr(frames + 8 + ps)? as u32;
    let mask = c.g.ptr(frames + 8 + 2 * ps)? as u32;
    let hx = c.g.u32(frames + 8 + 3 * ps)? as i32;
    let hy = c.g.u32(frames + 12 + 3 * ps)? as i32;
    let mut u = c.p.user.lock().unwrap();
    let Some(full) = u.full_handle(h, ObjKind::Icon) else {
        drop(u);
        set_last_error(c, err::INVALID_CURSOR_HANDLE);
        return Ok(0);
    };
    let o = u.input.icons.entry(full).or_default();
    o.module = module;
    o.res_name = res_name;
    o.width = width;
    o.height = height;
    o.color = color;
    o.alpha = alpha;
    o.mask = mask;
    o.hotspot = (hx, hy);
    o.rsrc = rsrc;
    o.has_data = true;
    Ok(1)
}

/// `NtUserFindExistingCursorIcon(module, res_name, rsrc)`: a shared
/// cursor or icon already loaded from that module's resource (Wine passes
/// the HRSRC as the third argument).
pub fn nt_user_find_existing_cursor_icon(c: &mut Call<'_>) -> R64 {
    let (module, rsrc) = (c.a(0), c.a(2));
    let module = us_name(c, module)?;
    let u = c.p.user.lock().unwrap();
    let found = u
        .input
        .icons
        .iter()
        .find(|(_, o)| o.has_data && !o.module.is_empty() && o.module == module && o.rsrc == rsrc)
        .map(|(&h, _)| h)
        .unwrap_or(0);
    Ok(handle_ret(found))
}

/// `NtUserDestroyCursor(cursor, arg)`.
pub fn nt_user_destroy_cursor(c: &mut Call<'_>) -> R64 {
    let h = c.a32(0);
    let mut u = c.p.user.lock().unwrap();
    let Some(full) = u.full_handle(h, ObjKind::Icon) else { return Ok(0) };
    // Shared (resource) cursors stay, as on Windows.
    if u.input.icons.get(&full).is_some_and(|o| !o.module.is_empty()) {
        return Ok(1);
    }
    u.input.icons.remove(&full);
    u.free_handle(full);
    Ok(1)
}

/// `NtUserGetIconSize(icon, step, width, height)`.
pub fn nt_user_get_icon_size(c: &mut Call<'_>) -> R64 {
    let (h, w_p, h_p) = (c.a32(0), c.a(2), c.a(3));
    let o = {
        let u = c.p.user.lock().unwrap();
        u.full_handle(h, ObjKind::Icon).and_then(|f| u.input.icons.get(&f).cloned())
    };
    let Some(o) = o else {
        set_last_error(c, err::INVALID_CURSOR_HANDLE);
        return Ok(0);
    };
    c.g.w32(w_p, o.width)?;
    c.g.w32(h_p, o.height)?;
    Ok(1)
}

/// `NtUserGetIconInfo(icon, info, module, res_name, bpp, unk)`.
pub fn nt_user_get_icon_info(c: &mut Call<'_>) -> R64 {
    let (h, info) = (c.a32(0), c.a(1));
    let o = {
        let u = c.p.user.lock().unwrap();
        u.full_handle(h, ObjKind::Icon).and_then(|f| u.input.icons.get(&f).cloned())
    };
    let Some(o) = o else {
        set_last_error(c, err::INVALID_CURSOR_HANDLE);
        return Ok(0);
    };
    if info != 0 {
        // ICONINFO { fIcon, xHotspot, yHotspot, hbmMask, hbmColor }
        let ps = c.p.abi.ptr_size();
        c.g.w32(info, o.is_icon as u32)?;
        c.g.w32(info + 4, o.hotspot.0 as u32)?;
        c.g.w32(info + 8, o.hotspot.1 as u32)?;
        c.g.wptr(info + 16 - if ps == 4 { 4 } else { 0 }, o.mask as u64)?;
        c.g.wptr(info + 16 - if ps == 4 { 4 } else { 0 } + ps, o.color as u64)?;
    }
    Ok(1)
}

/// `NtUserGetDoubleClickTime()`.
pub fn nt_user_get_double_click_time(_c: &mut Call<'_>) -> R64 {
    Ok(500)
}

/// `NtUserGetClipboardSequenceNumber()`: nothing was ever copied.
pub fn nt_user_get_clipboard_sequence_number(_c: &mut Call<'_>) -> R64 {
    Ok(0)
}

/// `NtUserAssociateInputContext(hwnd, himc, flags)`: there is no IME, so
/// no context to associate (AICR_OK).
pub fn nt_user_associate_input_context(_c: &mut Call<'_>) -> R64 {
    Ok(0)
}

/// `NtUserSetCapture(hwnd)`: the previous capture window.
pub fn nt_user_set_capture(c: &mut Call<'_>) -> R64 {
    let h = c.a32(0);
    let tid = c.t.tid;
    let mut u = c.p.user.lock().unwrap();
    let full = if h == 0 { 0 } else { window::full(&u, h) };
    Ok(handle_ret(std::mem::replace(&mut u.thread(tid).capture, full)))
}

/// `NtUserReleaseCapture()`.
pub fn nt_user_release_capture(c: &mut Call<'_>) -> R64 {
    let tid = c.t.tid;
    c.p.user.lock().unwrap().thread(tid).capture = 0;
    Ok(1)
}

/// `NtUserWindowFromPoint(x, y)`.
pub fn nt_user_window_from_point(c: &mut Call<'_>) -> R64 {
    let (x, y) = (c.a32(0) as i32, c.a32(1) as i32);
    let u = c.p.user.lock().unwrap();
    Ok(handle_ret(window_from_point(&u, (x, y))))
}

/// `NtUserTrackMouseEvent(info)`: TRACKMOUSEEVENT { cbSize, dwFlags,
/// hwndTrack, dwHoverTime }. TME_LEAVE, TME_CANCEL and TME_QUERY; hovering
/// is not tracked yet.
pub fn nt_user_track_mouse_event(c: &mut Call<'_>) -> R64 {
    const TME_HOVER: u32 = 0x1;
    const TME_LEAVE: u32 = 0x2;
    const TME_QUERY: u32 = 0x4000_0000;
    const TME_CANCEL: u32 = 0x8000_0000;
    let p = c.a(0);
    let flags = c.g.u32(p + 4)?;
    let h = c.g.ptr(p + 8)? as u32;
    let mut u = c.p.user.lock().unwrap();
    if flags & TME_QUERY != 0 {
        let t = u.input.track_leave;
        drop(u);
        c.g.w32(p + 4, if t != 0 { TME_LEAVE } else { 0 })?;
        c.g.wptr(p + 8, handle_ret(t))?;
        return Ok(1);
    }
    if flags & TME_HOVER != 0 && flags & TME_CANCEL == 0 {
        drop(u);
        return Err(c.unimplemented("TrackMouseEvent(TME_HOVER)"));
    }
    let full = window::full(&u, h);
    if flags & TME_LEAVE != 0 {
        if flags & TME_CANCEL != 0 {
            if u.input.track_leave == full {
                u.input.track_leave = 0;
            }
        } else {
            u.input.track_leave = full;
        }
    }
    Ok(1)
}

/// `NtUserSendInput(count, inputs, size)`: synthesized input, injected where
/// hardware input is. Absolute mouse coordinates are normalized to the
/// primary display (0..65535).
pub fn nt_user_send_input(c: &mut Call<'_>) -> R64 {
    let (count, inputs, size) = (c.a32(0), c.a(1), c.a32(2) as u64);
    let ps = c.p.abi.ptr_size();
    let expect = if ps == 8 { 40 } else { 28 };
    if size != expect {
        set_last_error(c, err::INVALID_PARAMETER);
        return Ok(0);
    }
    for i in 0..count as u64 {
        let at = inputs + i * size;
        let ty = c.g.u32(at)?;
        let u = at + ps; // the union, pointer-aligned
        match ty {
            // INPUT_MOUSE: MOUSEINPUT { dx, dy, mouseData, dwFlags, time, extra }
            0 => {
                let (dx, dy, data, flags) = (c.g.u32(u)? as i32, c.g.u32(u + 4)? as i32, c.g.u32(u + 8)? as i32, c.g.u32(u + 12)?);
                if flags & 0x1 != 0 {
                    let (cx, cy) = c.p.user.lock().unwrap().input.cursor;
                    let pt = if flags & 0x8000 != 0 {
                        let r = super::sysparams::primary_rect(c);
                        ((dx as i64 * r.width() as i64 / 65536) as i32, (dy as i64 * r.height() as i64 / 65536) as i32)
                    } else {
                        (cx + dx, cy + dy)
                    };
                    inject_mouse(c, pt, Mouse::Move);
                }
                let pt = c.p.user.lock().unwrap().input.cursor;
                for (bit, b, down) in [
                    (0x2, vk::LBUTTON, true),
                    (0x4, vk::LBUTTON, false),
                    (0x8, vk::RBUTTON, true),
                    (0x10, vk::RBUTTON, false),
                    (0x20, vk::MBUTTON, true),
                    (0x40, vk::MBUTTON, false),
                ] {
                    if flags & bit != 0 {
                        inject_mouse(c, pt, Mouse::Button(b, down));
                    }
                }
                if flags & 0x800 != 0 {
                    inject_mouse(c, pt, Mouse::Wheel(data));
                }
                if flags & !(0x1 | 0x2 | 0x4 | 0x8 | 0x10 | 0x20 | 0x40 | 0x800 | 0x8000) != 0 {
                    return Err(c.unimplemented(&format!("SendInput mouse flags {flags:#x}")));
                }
            }
            // INPUT_KEYBOARD: KEYBDINPUT { wVk, wScan, dwFlags, time, extra }
            1 => {
                let (v, scan, flags) = (c.g.u16(u)?, c.g.u16(u + 2)?, c.g.u32(u + 4)?);
                if flags & (0x4 | 0x8) != 0 {
                    return Err(c.unimplemented("SendInput with KEYEVENTF_UNICODE or KEYEVENTF_SCANCODE"));
                }
                // The scan code is the caller's, extended if it says so.
                let scan = scan as u32 | if flags & 0x1 != 0 { 0xe000 } else { 0 };
                inject_key(c, v as u8, flags & 0x2 == 0, Some(scan));
            }
            _ => return Err(c.unimplemented(&format!("SendInput type {ty}"))),
        }
    }
    Ok(count as u64)
}
