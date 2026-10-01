//! Window surfaces (docs/nt-layer.md §6a.1): each top-level window draws
//! into a 32 bpp top-down BGRA bitmap of its window rectangle's size, in
//! guest memory. Child windows draw into their top-level window's surface,
//! clipped to their ancestors' client areas.
//!
//! Publishing a surface to a display is the NtDisplay side; until it exists
//! a surface is only drawn into and read back (GetPixel, BitBlt from a
//! window DC).

use super::super::user::{self, window::ws, window::Rect};
use super::bitmap::{self, Bitmap, Dib};
use super::{alloc, free, get, Obj};
use crate::nt::{Call, R};

/// Where a window DC draws.
pub struct Geometry {
    /// The top-level window whose surface it is.
    pub toplevel: u32,
    /// The DC's device origin, in surface coordinates.
    pub origin: (i32, i32),
    /// The DC's origin in screen coordinates.
    pub screen: (i32, i32),
    /// What it may draw on, in surface coordinates.
    pub vis: Vec<Rect>,
    /// The surface's size (the top-level window's).
    pub size: (i32, i32),
}

/// A window DC's geometry, or `None` for the desktop and message-only
/// windows (which have no surface).
pub fn geometry(c: &Call<'_>, hwnd: u32, whole: bool) -> Option<Geometry> {
    let u = c.p.user.lock().unwrap();
    let w = user::window::find(&u, hwnd)?;
    if w.hwnd == u.desktop {
        return None;
    }
    // Up to the top-level window, noting visibility and ancestors' client
    // areas.
    let mut visible = w.style & ws::VISIBLE != 0;
    let mut clips = Vec::new();
    let mut t = w;
    while t.style & ws::CHILD != 0 && t.parent != u.desktop {
        let p = u.windows.get(&t.parent)?;
        visible &= p.style & ws::VISIBLE != 0;
        clips.push(user::window::screen_client_rect(&u, p));
        t = p;
    }
    if t.parent != u.desktop {
        return None; // message-only
    }
    let tr = user::window::screen_window_rect(&u, t);
    let area = if whole { user::window::screen_window_rect(&u, w) } else { user::window::screen_client_rect(&u, w) };
    let mut vis = area;
    for cl in &clips {
        vis = vis.intersect(cl);
    }
    vis = vis.intersect(&tr).offset(-tr.left, -tr.top);
    Some(Geometry {
        toplevel: t.hwnd,
        origin: (area.left - tr.left, area.top - tr.top),
        screen: (area.left, area.top),
        vis: if visible && !vis.is_empty() { vec![vis] } else { Vec::new() },
        size: (tr.width().max(0), tr.height().max(0)),
    })
}

/// A 32 bpp top-down DIB-section-like bitmap of `w` x `h`.
fn new_surface(c: &mut Call<'_>, w: i32, h: i32) -> R<Bitmap> {
    let stride = bitmap::dib_stride(w, 32);
    let size = (stride as u64 * h as u64).max(1);
    let bits = crate::nt::sysinfo::alloc_private(c, size, "[window surface]")?;
    let mut header = vec![0u8; 40];
    header[0..4].copy_from_slice(&40u32.to_le_bytes());
    header[4..8].copy_from_slice(&w.to_le_bytes());
    header[8..12].copy_from_slice(&(-h).to_le_bytes());
    header[12..14].copy_from_slice(&1u16.to_le_bytes());
    header[14..16].copy_from_slice(&32u16.to_le_bytes());
    Ok(Bitmap {
        width: w,
        height: h,
        bpp: 32,
        planes: 1,
        stride,
        bits,
        alloc: size,
        dib: Some(Dib { top_down: true, compression: 0, masks: [0xff0000, 0xff00, 0xff], colors: Vec::new(), header }),
        selected: false,
    })
}

/// The surface of top-level window `toplevel`, sized `size`: made on first
/// use and remade (keeping what overlaps) when the window's size changed.
pub fn surface(c: &mut Call<'_>, toplevel: u32, size: (i32, i32)) -> R<Bitmap> {
    let cur = c.p.user.lock().unwrap().windows.get(&toplevel).map(|w| w.surface).unwrap_or(0);
    if let Some(Obj::Bitmap(b)) = get(c, cur) {
        if (b.width, b.height) == size {
            return Ok(b);
        }
    }
    let nb = new_surface(c, size.0.max(1), size.1.max(1))?;
    if let Some(Obj::Bitmap(old)) = get(c, cur) {
        let rows = old.height.min(nb.height);
        let bytes = (old.width.min(nb.width) * 4) as usize;
        for y in 0..rows {
            let row = c.g.bytes(old.row(y), bytes)?;
            c.g.write(nb.row(y), &row)?;
        }
        if let Some(Obj::Bitmap(b)) = free(c, cur)? {
            b.release(c)?;
        }
    }
    let h = alloc(c, Obj::Bitmap(nb.clone()))?;
    if let Some(w) = c.p.user.lock().unwrap().windows.get_mut(&toplevel) {
        w.surface = h;
    }
    Ok(nb)
}

/// A destroyed window's surface and own DC go with it.
pub fn release_window(c: &mut Call<'_>, surface: u32, own_dc: u32) -> R<()> {
    if surface != 0 {
        if let Some(Obj::Bitmap(b)) = free(c, surface)? {
            b.release(c)?;
        }
    }
    if own_dc != 0 {
        if let Some(Obj::Dc(d)) = free(c, own_dc)? {
            super::dc::release(c, d)?;
        }
    }
    Ok(())
}

/// Hand a top-level window's surface to the display (§6a.1).
pub fn flush(c: &mut Call<'_>, toplevel: u32) {
    crate::nt::display::publish(c, toplevel);
}
