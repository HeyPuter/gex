//! The display side (docs/nt-layer.md §6a.1): top-level windows' surfaces
//! go to whatever displays them through [`NtDisplay`], and that display's
//! input comes back as hardware input.
//!
//! The NT layer knows nothing about Wayland or the browser: a host gives the
//! process an `NtDisplay` (the native shim's composites to PPM files or a GPU
//! compositor; the browser's writes the compositor's scene records), and the
//! surfaces it publishes are guest memory it reads in place. Without one,
//! windows are drawn but shown nowhere, and input is only what
//! `GEX_NT_INPUT` scripts.

use super::gdi::{self, Obj};
use super::user::{self, input, window};
use super::Call;
use crate::GuestMem;

/// One top-level window's surface, as the display sees it.
#[derive(Clone, Debug)]
pub struct NtSurface {
    /// The window (its full handle): stable for the window's life.
    pub id: u32,
    /// Guest address of the first row of 32 bpp BGRX pixels, top-down.
    pub addr: u64,
    pub stride: u32,
    pub width: i32,
    pub height: i32,
    /// What changed since the last publish, in surface coordinates
    /// (left, top, right, bottom); the whole surface when unknown.
    pub damage: (i32, i32, i32, i32),
    pub title: String,
    /// Where the window is on the screen.
    pub x: i32,
    pub y: i32,
}

/// Input and window events from the display, addressed to a window.
#[derive(Clone, Copy, Debug)]
pub enum NtDisplayEvent {
    /// The pointer at (x, y) in the window's surface coordinates.
    Motion { window: u32, x: i32, y: i32 },
    /// A button, by evdev code (BTN_LEFT 0x110, BTN_RIGHT, BTN_MIDDLE).
    Button { window: u32, button: u32, pressed: bool },
    /// Scrolling: axis 0 vertical, 1 horizontal; `value` in wheel clicks
    /// times 120 (WHEEL_DELTA), positive away from the user.
    Axis { window: u32, axis: u32, value: i32 },
    /// A key, by evdev code.
    Key { window: u32, code: u32, pressed: bool },
    /// The window gained the keyboard focus.
    Focus { window: u32 },
    /// The display resized the window.
    Configure { window: u32, width: i32, height: i32 },
    /// The user asked to close the window.
    Close { window: u32 },
}

pub trait NtDisplay: Send + Sync {
    /// Show `surface` (new or changed). The pixels are guest memory, valid
    /// for the duration of the call.
    fn publish(&self, mem: &GuestMem<'_>, surface: &NtSurface);
    /// The window is gone or hidden.
    fn remove(&self, id: u32);
    /// Input and window events since the last call.
    fn poll_input(&self) -> Vec<NtDisplayEvent>;
}

/// Publish top-level window `toplevel`'s surface, if there is a display.
pub fn publish(c: &mut Call<'_>, toplevel: u32) {
    let Some(d) = c.p.display() else { return };
    let (surface, title, rect, visible) = {
        let u = c.p.user.lock().unwrap();
        let Some(w) = u.windows.get(&toplevel) else { return };
        (
            w.surface,
            super::wstr(&w.text),
            window::screen_window_rect(&u, w),
            w.style & window::ws::VISIBLE != 0,
        )
    };
    if !visible {
        d.remove(toplevel);
        return;
    }
    let Some(Obj::Bitmap(b)) = gdi::get(c, surface) else { return };
    let s = NtSurface {
        id: toplevel,
        addr: b.bits,
        stride: b.stride,
        width: b.width,
        height: b.height,
        damage: (0, 0, b.width, b.height),
        title,
        x: rect.left,
        y: rect.top,
    };
    d.publish(&c.g.m, &s);
}

/// A window the display should stop showing.
pub fn remove(c: &Call<'_>, toplevel: u32) {
    if let Some(d) = c.p.display() {
        d.remove(toplevel);
    }
}

/// evdev key code to the PC scan code (0xe0nn for extended keys).
fn evdev_to_scan(code: u32) -> u32 {
    match code {
        // Keys 1..=88 (Esc .. F12) are their set-1 scan codes.
        1..=88 => code,
        96 => 0xe01c,  // KP Enter
        97 => 0xe01d,  // Right Ctrl
        98 => 0xe035,  // KP /
        99 => 0xe037,  // SysRq / Print Screen
        100 => 0xe038, // Right Alt
        102 => 0xe047, // Home
        103 => 0xe048, // Up
        104 => 0xe049, // Page Up
        105 => 0xe04b, // Left
        106 => 0xe04d, // Right
        107 => 0xe04f, // End
        108 => 0xe050, // Down
        109 => 0xe051, // Page Down
        110 => 0xe052, // Insert
        111 => 0xe053, // Delete
        119 => 0x45,   // Pause (as Num Lock's scan with E1 in reality)
        125 => 0xe05b, // Left Meta
        126 => 0xe05c, // Right Meta
        127 => 0xe05d, // Menu
        _ => 0,
    }
}

/// The virtual key for an evdev code on the US layout.
pub fn evdev_to_vk(code: u32) -> (u8, u32) {
    let scan = evdev_to_scan(code);
    let idx = if scan & 0xe000 != 0 { 0x100 + (scan & 0xff) } else { scan } as usize;
    let vk = user::kbdus::VSC2VK.get(idx).map(|&v| (v & 0xff) as u8).unwrap_or(0);
    (vk, scan)
}

/// Take the display's pending input and turn it into hardware input.
pub fn pump_input(c: &mut Call<'_>) {
    let Some(d) = c.p.display() else { return };
    for ev in d.poll_input() {
        match ev {
            NtDisplayEvent::Motion { window, x, y } => {
                let at = origin(c, window).map(|(ox, oy)| (ox + x, oy + y));
                if let Some(pt) = at {
                    input::inject_mouse(c, pt, input::Mouse::Move);
                }
            }
            NtDisplayEvent::Button { button, pressed, .. } => {
                let b = match button {
                    0x110 => input::vk::LBUTTON,
                    0x111 => input::vk::RBUTTON,
                    0x112 => input::vk::MBUTTON,
                    _ => continue,
                };
                let pt = c.p.user.lock().unwrap().input.cursor;
                input::inject_mouse(c, pt, input::Mouse::Button(b, pressed));
            }
            NtDisplayEvent::Axis { axis, value, .. } => {
                if axis == 0 {
                    let pt = c.p.user.lock().unwrap().input.cursor;
                    input::inject_mouse(c, pt, input::Mouse::Wheel(value));
                }
            }
            NtDisplayEvent::Key { code, pressed, .. } => {
                let (vk, scan) = evdev_to_vk(code);
                if vk != 0 {
                    input::inject_key(c, vk, pressed, Some(scan));
                }
            }
            NtDisplayEvent::Focus { window } => {
                let w = user::window::full(&c.p.user.lock().unwrap(), window);
                if w != 0 {
                    let _ = user::window::set_active_window(c, w, false);
                }
            }
            NtDisplayEvent::Configure { window, width, height } => {
                let w = user::window::full(&c.p.user.lock().unwrap(), window);
                if w != 0 {
                    let _ = user::window::set_window_pos(c, w, 0, 0, 0, width, height, 0x2 | 0x4 | 0x10);
                }
            }
            NtDisplayEvent::Close { window } => {
                let w = user::window::full(&c.p.user.lock().unwrap(), window);
                let tid = c.p.user.lock().unwrap().windows.get(&w).map(|x| x.tid);
                if let Some(tid) = tid {
                    // WM_SYSCOMMAND(SC_CLOSE), as a click on the frame's close
                    // button posts it.
                    let time = user::message::tick_count(c);
                    let m = user::message::Msg { hwnd: w, msg: 0x112, wparam: 0xf060, lparam: 0, time, pt: (0, 0) };
                    user::message::post_to_thread(c, tid, m);
                }
            }
        }
    }
}

/// A window's screen origin (its window rectangle's top left).
fn origin(c: &Call<'_>, window: u32) -> Option<(i32, i32)> {
    let u = c.p.user.lock().unwrap();
    let w = user::window::find(&u, window)?;
    let r = user::window::screen_window_rect(&u, w);
    Some((r.left, r.top))
}
