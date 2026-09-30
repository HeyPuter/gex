//! Blits: BitBlt, StretchBlt, PatBlt, StretchDIBits, SetDIBitsToDevice and
//! pixel access, with every ternary raster operation (win32u's bitblt.c and
//! the dib engine's blits, one pixel at a time for now).

use super::super::user::{window::Rect, R64};
use super::bitmap::{self, Bitmap};
use super::{get, Obj};
use crate::nt::{off, Call, Status, R};

/// Where a DC draws: its bitmap (memory DCs) and the offset of its origin.
struct Target {
    bm: Bitmap,
    /// Device origin in the bitmap (window DCs inside a surface).
    dx: i32,
    dy: i32,
    clip: Rect,
    attr: u64,
    brush: u32,
    mono: bool,
}

fn target(c: &mut Call<'_>, hdc: u32) -> R<Option<Target>> {
    let Some(Obj::Dc(d)) = get(c, hdc) else { return Ok(None) };
    if !d.memory {
        return Err(c.unimplemented("drawing to a window or display DC (window surfaces are not built yet)"));
    }
    let Some(Obj::Bitmap(bm)) = get(c, d.bitmap) else { return Ok(None) };
    let map_mode = c.g.u32(d.attr + off!(c.p.abi, dc_attr::MAP_MODE))?;
    if map_mode != 1 {
        return Err(c.unimplemented(&format!("drawing with map mode {map_mode}")));
    }
    let abi = c.p.abi;
    let wx = c.g.u32(d.attr + off!(abi, dc_attr::WND_ORG))? as i32;
    let wy = c.g.u32(d.attr + off!(abi, dc_attr::WND_ORG) + 4)? as i32;
    let vx = c.g.u32(d.attr + off!(abi, dc_attr::VPORT_ORG))? as i32;
    let vy = c.g.u32(d.attr + off!(abi, dc_attr::VPORT_ORG) + 4)? as i32;
    let full = Rect { left: 0, top: 0, right: bm.width, bottom: bm.height };
    let clip = d.clip.map(|r| r.intersect(&full)).unwrap_or(full);
    Ok(Some(Target { mono: bm.bpp == 1, bm, dx: vx - wx, dy: vy - wy, clip, attr: d.attr, brush: d.brush }))
}

/// The pattern color at device (x, y), or `None` for a null brush.
struct Pattern {
    solid: Option<u32>,
    bitmap: Option<Bitmap>,
    org: (i32, i32),
}

fn pattern(c: &mut Call<'_>, t: &Target) -> R<Pattern> {
    let abi = c.p.abi;
    let org = (c.g.u32(t.attr + off!(abi, dc_attr::BRUSH_ORG))? as i32, c.g.u32(t.attr + off!(abi, dc_attr::BRUSH_ORG) + 4)? as i32);
    let dc_brush = super::stock(c, super::stock::DC_BRUSH);
    match get(c, t.brush) {
        Some(Obj::Brush(b)) => match b.style {
            0 => {
                let color = if t.brush == dc_brush { c.g.u32(t.attr + off!(abi, dc_attr::BRUSH_COLOR))? } else { b.color };
                Ok(Pattern { solid: Some(colorref_to_rgb(color)), bitmap: None, org })
            }
            1 => Ok(Pattern { solid: None, bitmap: None, org }),
            3 => {
                let bm = match b.pattern.and_then(|p| get(c, p)) {
                    Some(Obj::Bitmap(bm)) => bm,
                    _ => return Ok(Pattern { solid: None, bitmap: None, org }),
                };
                Ok(Pattern { solid: None, bitmap: Some(bm), org })
            }
            s => Err(c.unimplemented(&format!("drawing with brush style {s}"))),
        },
        _ => Ok(Pattern { solid: None, bitmap: None, org }),
    }
}

impl Pattern {
    fn at(&self, c: &Call<'_>, x: i32, y: i32) -> Option<u32> {
        if let Some(s) = self.solid {
            return Some(s);
        }
        let b = self.bitmap.as_ref()?;
        let px = (x - self.org.0).rem_euclid(b.width.max(1));
        let py = (y - self.org.1).rem_euclid(b.height.max(1));
        Some(b.get(c, px, py))
    }
}

/// COLORREF (0x00BBGGRR) to the pixel order bitmaps use (0x00RRGGBB).
pub fn colorref_to_rgb(c: u32) -> u32 {
    (c & 0xff) << 16 | (c & 0xff00) | (c >> 16 & 0xff)
}

pub fn rgb_to_colorref(c: u32) -> u32 {
    colorref_to_rgb(c)
}

/// A ternary raster operation on 24-bit colors.
fn rop3(code: u8, p: u32, s: u32, d: u32) -> u32 {
    let mut r = 0u32;
    for i in 0..8 {
        if code & (1 << i) != 0 {
            let pp = if i & 4 != 0 { p } else { !p };
            let ss = if i & 2 != 0 { s } else { !s };
            let dd = if i & 1 != 0 { d } else { !d };
            r |= pp & ss & dd;
        }
    }
    r & 0xffffff
}

fn uses_source(code: u8) -> bool {
    ((code >> 2) ^ code) & 0x33 != 0
}

fn uses_pattern(code: u8) -> bool {
    ((code >> 4) ^ code) & 0x0f != 0
}

/// A source for a blit: a bitmap, its origin offset, and how monochrome
/// pixels map to colors.
struct Source {
    bm: Bitmap,
    dx: i32,
    dy: i32,
    /// For a monochrome source into color: (0 → text color, 1 → background).
    mono_colors: Option<(u32, u32)>,
    /// For a color source into monochrome: the source DC's background.
    to_mono_bk: Option<u32>,
}

/// The general stretching blit, nearest neighbor, mirroring on negative
/// sizes. Device coordinates.
#[allow(clippy::too_many_arguments)]
fn blit(c: &mut Call<'_>, t: &Target, xd: i32, yd: i32, wd: i32, hd: i32, src: Option<&Source>, xs: i32, ys: i32, ws: i32, hs: i32, rop: u32) -> R<()> {
    let code = (rop >> 16) as u8;
    let pat = if uses_pattern(code) { Some(pattern(c, t)?) } else { None };
    if let Some(p) = &pat {
        if p.solid.is_none() && p.bitmap.is_none() {
            // A null brush draws nothing.
            return Ok(());
        }
    }
    let (x0, x1) = if wd < 0 { (xd + wd, xd) } else { (xd, xd + wd) };
    let (y0, y1) = if hd < 0 { (yd + hd, yd) } else { (yd, yd + hd) };
    let area = Rect { left: x0 + t.dx, top: y0 + t.dy, right: x1 + t.dx, bottom: y1 + t.dy }.intersect(&t.clip);
    if area.is_empty() {
        return Ok(());
    }
    let (awd, ahd) = (wd.abs().max(1), hd.abs().max(1));
    for y in area.top..area.bottom {
        for x in area.left..area.right {
            let s = match src {
                Some(s) if uses_source(code) => {
                    // Position within the destination rectangle, then the
                    // matching source pixel.
                    let mut fx = x - t.dx - x0;
                    let mut fy = y - t.dy - y0;
                    if wd < 0 {
                        fx = awd - 1 - fx;
                    }
                    if hd < 0 {
                        fy = ahd - 1 - fy;
                    }
                    let sx = xs + (fx as i64 * ws as i64 / awd as i64) as i32 + s.dx;
                    let sy = ys + (fy as i64 * hs as i64 / ahd as i64) as i32 + s.dy;
                    if sx < 0 || sy < 0 || sx >= s.bm.width || sy >= s.bm.height {
                        continue;
                    }
                    let v = s.bm.get(c, sx, sy);
                    match (s.mono_colors, s.to_mono_bk) {
                        (Some((fg, bg)), _) => {
                            if v != 0 {
                                bg
                            } else {
                                fg
                            }
                        }
                        (None, Some(bk)) => {
                            if v == bk {
                                0xffffff
                            } else {
                                0
                            }
                        }
                        _ => v,
                    }
                }
                _ => 0,
            };
            let p = pat.as_ref().and_then(|p| p.at(c, x, y)).unwrap_or(0);
            let d = t.bm.get(c, x, y);
            let r = rop3(code, p, s, d);
            t.bm.set(c, x, y, r);
        }
    }
    Ok(())
}

fn dc_colors(c: &Call<'_>, attr: u64) -> R<(u32, u32)> {
    let abi = c.p.abi;
    Ok((
        colorref_to_rgb(c.g.u32(attr + off!(abi, dc_attr::TEXT_COLOR))?),
        colorref_to_rgb(c.g.u32(attr + off!(abi, dc_attr::BACKGROUND_COLOR))?),
    ))
}

fn source_from_dc(c: &mut Call<'_>, hdc: u32, dst: &Target) -> R<Option<Source>> {
    let Some(s) = target(c, hdc)? else { return Ok(None) };
    let mono_colors = if s.mono && !dst.mono { Some(dc_colors(c, dst.attr)?) } else { None };
    let to_mono_bk = if !s.mono && dst.mono { Some(dc_colors(c, s.attr)?.1) } else { None };
    Ok(Some(Source { bm: s.bm, dx: s.dx, dy: s.dy, mono_colors, to_mono_bk }))
}

/// `NtGdiBitBlt(hdc_dst, x, y, w, h, hdc_src, xs, ys, rop, bk_color, fl)`.
pub fn nt_gdi_bit_blt(c: &mut Call<'_>) -> R64 {
    let (hd, x, y, w, h, hs, xs, ys, rop) =
        (c.a32(0), c.a32(1) as i32, c.a32(2) as i32, c.a32(3) as i32, c.a32(4) as i32, c.a32(5), c.a32(6) as i32, c.a32(7) as i32, c.a32(8));
    let Some(t) = target(c, hd)? else { return Ok(0) };
    let src = if uses_source((rop >> 16) as u8) { source_from_dc(c, hs, &t)? } else { None };
    if uses_source((rop >> 16) as u8) && src.is_none() {
        return Ok(0);
    }
    blit(c, &t, x, y, w, h, src.as_ref(), xs, ys, w, h, rop)?;
    Ok(1)
}

/// `NtGdiStretchBlt(hdc_dst, x, y, w, h, hdc_src, xs, ys, ws, hs, rop, bk)`.
pub fn nt_gdi_stretch_blt(c: &mut Call<'_>) -> R64 {
    let (hd, x, y, w, h, hs) = (c.a32(0), c.a32(1) as i32, c.a32(2) as i32, c.a32(3) as i32, c.a32(4) as i32, c.a32(5));
    let (xs, ys, ws, hsz, rop) = (c.a32(6) as i32, c.a32(7) as i32, c.a32(8) as i32, c.a32(9) as i32, c.a32(10));
    let Some(t) = target(c, hd)? else { return Ok(0) };
    let src = if uses_source((rop >> 16) as u8) { source_from_dc(c, hs, &t)? } else { None };
    if uses_source((rop >> 16) as u8) && src.is_none() {
        return Ok(0);
    }
    // A negative source size mirrors too: fold it into the destination.
    let (x, w, xs, ws) = if ws < 0 { (x + w, -w, xs + ws, -ws) } else { (x, w, xs, ws) };
    let (y, h, ys, hsz) = if hsz < 0 { (y + h, -h, ys + hsz, -hsz) } else { (y, h, ys, hsz) };
    blit(c, &t, x, y, w, h, src.as_ref(), xs, ys, ws, hsz, rop)?;
    Ok(1)
}

/// `NtGdiPatBlt(hdc, x, y, w, h, rop)`.
pub fn nt_gdi_pat_blt(c: &mut Call<'_>) -> R64 {
    let (hd, x, y, w, h, rop) = (c.a32(0), c.a32(1) as i32, c.a32(2) as i32, c.a32(3) as i32, c.a32(4) as i32, c.a32(5));
    let Some(t) = target(c, hd)? else { return Ok(0) };
    if uses_source((rop >> 16) as u8) {
        return Ok(0);
    }
    blit(c, &t, x, y, w, h, None, 0, 0, 0, 0, rop)?;
    Ok(1)
}

/// `NtGdiStretchDIBitsInternal(hdc, xd, yd, wd, hd, xs, ys, ws, hs, bits,
/// bmi, coloruse, rop, max_info, max_bits, xform)`: lines copied.
pub fn nt_gdi_stretch_dibits_internal(c: &mut Call<'_>) -> R64 {
    let (hdc, xd, yd, wd, hd) = (c.a32(0), c.a32(1) as i32, c.a32(2) as i32, c.a32(3) as i32, c.a32(4) as i32);
    let (xs, ys, ws, hs, bits, bmi, usage, rop) =
        (c.a32(5) as i32, c.a32(6) as i32, c.a32(7) as i32, c.a32(8) as i32, c.a(9), c.a(10), c.a32(11), c.a32(12));
    let Some(t) = target(c, hdc)? else { return Ok(0) };
    let info = bitmap::read_info(c, bmi, usage)?;
    let bm = bitmap::client_dib(&info, bits);
    // DIB source coordinates count rows from the bottom for bottom-up
    // images.
    let ys_top = if info.height > 0 { bm.height - ys - hs } else { ys };
    let mono_colors = None;
    let src = Source { bm, dx: 0, dy: 0, mono_colors, to_mono_bk: None };
    blit(c, &t, xd, yd, wd, hd, Some(&src), xs, ys_top, ws, hs, rop)?;
    Ok(info.height.unsigned_abs() as u64)
}

/// `NtGdiSetDIBitsToDeviceInternal(hdc, xd, yd, cx, cy, xs, ys, startscan,
/// lines, bits, bmi, coloruse, max_bits, max_info, xform_coords, xform)`.
pub fn nt_gdi_set_dibits_to_device_internal(c: &mut Call<'_>) -> R64 {
    let (hdc, xd, yd, cx, cy, xs, ys) =
        (c.a32(0), c.a32(1) as i32, c.a32(2) as i32, c.a32(3) as i32, c.a32(4) as i32, c.a32(5) as i32, c.a32(6) as i32);
    let (start, lines, bits, bmi, usage) = (c.a32(7) as i32, c.a32(8) as i32, c.a(9), c.a(10), c.a32(11));
    let Some(t) = target(c, hdc)? else { return Ok(0) };
    let info = bitmap::read_info(c, bmi, usage)?;
    if info.height < 0 {
        return Err(c.unimplemented("SetDIBitsToDevice with a top-down DIB"));
    }
    // The bits hold `lines` scan lines starting at `start` (bottom-up).
    let mut part = info;
    part.height = lines;
    let bm = bitmap::client_dib(&part, bits);
    // Source rows: DIB row r (from the bottom) is part row r - start.
    let top_of_src = ys + cy; // rows from the bottom
    let ys_in_part = lines - (top_of_src - start);
    let src = Source { bm, dx: 0, dy: 0, mono_colors: None, to_mono_bk: None };
    blit(c, &t, xd, yd, cx, cy, Some(&src), xs, ys_in_part, cx, cy, 0x00cc_0020)?;
    Ok(lines as u64)
}

/// `NtGdiGetPixel(hdc, x, y)`: a COLORREF, or CLR_INVALID.
pub fn nt_gdi_get_pixel(c: &mut Call<'_>) -> R64 {
    let (hdc, x, y) = (c.a32(0), c.a32(1) as i32, c.a32(2) as i32);
    let Some(t) = target(c, hdc)? else { return Ok(0xffff_ffff) };
    let (px, py) = (x + t.dx, y + t.dy);
    if px < t.clip.left || py < t.clip.top || px >= t.clip.right || py >= t.clip.bottom {
        return Ok(0xffff_ffff);
    }
    Ok(rgb_to_colorref(t.bm.get(c, px, py)) as u64)
}

/// `NtGdiSetPixel(hdc, x, y, color)`: the color set.
pub fn nt_gdi_set_pixel(c: &mut Call<'_>) -> R64 {
    let (hdc, x, y, color) = (c.a32(0), c.a32(1) as i32, c.a32(2) as i32, c.a32(3));
    let Some(t) = target(c, hdc)? else { return Ok(0xffff_ffff) };
    let (px, py) = (x + t.dx, y + t.dy);
    if px < t.clip.left || py < t.clip.top || px >= t.clip.right || py >= t.clip.bottom {
        return Ok(0xffff_ffff);
    }
    t.bm.set(c, px, py, colorref_to_rgb(color));
    Ok(rgb_to_colorref(t.bm.get(c, px, py)) as u64)
}

#[allow(dead_code)]
fn _unused(_: Status) {}
