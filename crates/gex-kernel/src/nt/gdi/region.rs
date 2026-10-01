//! Regions as lists of disjoint rectangles (win32u's region.c, without its
//! banded form): creation, combination, offsets, boxes and DC clipping.

use super::super::user::{handle_ret, read_rect, window::Rect, write_rect, R64};
use super::{alloc, get, with_obj, Obj};
use crate::nt::{Call, R};

pub const ERROR: u64 = 0;
pub const NULLREGION: u64 = 1;
pub const SIMPLEREGION: u64 = 2;
pub const COMPLEXREGION: u64 = 3;

/// `RGN_*` combine modes.
pub const RGN_AND: u32 = 1;
pub const RGN_OR: u32 = 2;
pub const RGN_XOR: u32 = 3;
pub const RGN_DIFF: u32 = 4;
pub const RGN_COPY: u32 = 5;

/// `a` minus `b`: up to four rectangles.
fn subtract_rect(a: &Rect, b: &Rect) -> Vec<Rect> {
    let i = a.intersect(b);
    if i.is_empty() {
        return vec![*a];
    }
    let parts = [
        Rect { left: a.left, top: a.top, right: a.right, bottom: i.top },
        Rect { left: a.left, top: i.bottom, right: a.right, bottom: a.bottom },
        Rect { left: a.left, top: i.top, right: i.left, bottom: i.bottom },
        Rect { left: i.right, top: i.top, right: a.right, bottom: i.bottom },
    ];
    parts.into_iter().filter(|r| !r.is_empty()).collect()
}

pub fn subtract(a: &[Rect], b: &[Rect]) -> Vec<Rect> {
    let mut out: Vec<Rect> = a.to_vec();
    for rb in b {
        out = out.iter().flat_map(|ra| subtract_rect(ra, rb)).collect();
    }
    out
}

pub fn intersect(a: &[Rect], b: &[Rect]) -> Vec<Rect> {
    let mut out = Vec::new();
    for ra in a {
        for rb in b {
            let i = ra.intersect(rb);
            if !i.is_empty() {
                out.push(i);
            }
        }
    }
    coalesce(out)
}

pub fn union(a: &[Rect], b: &[Rect]) -> Vec<Rect> {
    let mut out = a.to_vec();
    out.extend(subtract(b, a));
    coalesce(out)
}

/// Merge neighbors that together make a rectangle, so a region that is one
/// rectangle is represented as one.
pub fn coalesce(mut v: Vec<Rect>) -> Vec<Rect> {
    v.retain(|r| !r.is_empty());
    loop {
        let mut merged = false;
        'outer: for i in 0..v.len() {
            for j in i + 1..v.len() {
                let (a, b) = (v[i], v[j]);
                let m = if a.top == b.top && a.bottom == b.bottom && (a.right == b.left || b.right == a.left) {
                    Some(Rect { left: a.left.min(b.left), top: a.top, right: a.right.max(b.right), bottom: a.bottom })
                } else if a.left == b.left && a.right == b.right && (a.bottom == b.top || b.bottom == a.top) {
                    Some(Rect { left: a.left, top: a.top.min(b.top), right: a.right, bottom: a.bottom.max(b.bottom) })
                } else {
                    None
                };
                if let Some(m) = m {
                    v[i] = m;
                    v.remove(j);
                    merged = true;
                    break 'outer;
                }
            }
        }
        if !merged {
            break;
        }
    }
    v.sort_by_key(|r| (r.top, r.left));
    v
}

pub fn bounds(v: &[Rect]) -> Rect {
    v.iter().fold(Rect::default(), |acc, r| acc.union(r))
}

pub fn kind(v: &[Rect]) -> u64 {
    match v.len() {
        0 => NULLREGION,
        1 => SIMPLEREGION,
        _ => COMPLEXREGION,
    }
}

/// Combine two rectangle lists with an `RGN_*` mode.
pub fn combine(a: &[Rect], b: &[Rect], mode: u32) -> Option<Vec<Rect>> {
    Some(match mode {
        RGN_AND => intersect(a, b),
        RGN_OR => union(a, b),
        RGN_XOR => union(&subtract(a, b), &subtract(b, a)),
        RGN_DIFF => coalesce(subtract(a, b)),
        RGN_COPY => a.to_vec(),
        _ => return None,
    })
}

/// A normalized rectangle from possibly swapped corners.
pub fn ordered(l: i32, t: i32, r: i32, b: i32) -> Rect {
    Rect { left: l.min(r), top: t.min(b), right: l.max(r), bottom: t.max(b) }
}

/// The rectangles of region `h`.
pub fn rects(c: &Call<'_>, h: u32) -> Option<Vec<Rect>> {
    match get(c, h) {
        Some(Obj::Region(v)) => Some(v),
        _ => None,
    }
}

pub fn set(c: &Call<'_>, h: u32, v: Vec<Rect>) -> bool {
    with_obj(c, h, |o| {
        if let Obj::Region(r) = o {
            *r = v;
            true
        } else {
            false
        }
    })
    .unwrap_or(false)
}

/// `NtGdiCreateRectRgn(left, top, right, bottom)`.
pub fn nt_gdi_create_rect_rgn(c: &mut Call<'_>) -> R64 {
    let r = ordered(c.a32(0) as i32, c.a32(1) as i32, c.a32(2) as i32, c.a32(3) as i32);
    Ok(handle_ret(alloc(c, Obj::Region(coalesce(vec![r])))?))
}

/// `NtGdiSetRectRgn(rgn, left, top, right, bottom)`.
pub fn nt_gdi_set_rect_rgn(c: &mut Call<'_>) -> R64 {
    let h = c.a32(0);
    let r = ordered(c.a32(1) as i32, c.a32(2) as i32, c.a32(3) as i32, c.a32(4) as i32);
    Ok(set(c, h, coalesce(vec![r])) as u64)
}

/// `NtGdiGetRgnBox(rgn, *rect)`: the region's kind.
pub fn nt_gdi_get_rgn_box(c: &mut Call<'_>) -> R64 {
    let (h, p) = (c.a32(0), c.a(1));
    let Some(v) = rects(c, h) else { return Ok(ERROR) };
    write_rect(&mut c.g, p, &bounds(&v))?;
    Ok(kind(&v))
}

/// `NtGdiCombineRgn(dest, src1, src2, mode)`.
pub fn nt_gdi_combine_rgn(c: &mut Call<'_>) -> R64 {
    let (d, s1, s2, mode) = (c.a32(0), c.a32(1), c.a32(2), c.a32(3));
    let Some(a) = rects(c, s1) else { return Ok(ERROR) };
    let b = if mode == RGN_COPY { Vec::new() } else { rects(c, s2).unwrap_or_default() };
    if mode != RGN_COPY && rects(c, s2).is_none() {
        return Ok(ERROR);
    }
    let Some(v) = combine(&a, &b, mode) else { return Ok(ERROR) };
    let k = kind(&v);
    if !set(c, d, v) {
        return Ok(ERROR);
    }
    Ok(k)
}

/// `NtGdiOffsetRgn(rgn, x, y)`.
pub fn nt_gdi_offset_rgn(c: &mut Call<'_>) -> R64 {
    let (h, x, y) = (c.a32(0), c.a32(1) as i32, c.a32(2) as i32);
    let Some(v) = rects(c, h) else { return Ok(ERROR) };
    let v: Vec<Rect> = v.iter().map(|r| r.offset(x, y)).collect();
    let k = kind(&v);
    set(c, h, v);
    Ok(k)
}

/// `NtGdiEqualRgn(a, b)`.
pub fn nt_gdi_equal_rgn(c: &mut Call<'_>) -> R64 {
    let (Some(a), Some(b)) = (rects(c, c.a32(0)), rects(c, c.a32(1))) else { return Ok(0) };
    Ok((subtract(&a, &b).is_empty() && subtract(&b, &a).is_empty()) as u64)
}

/// `NtGdiPtInRegion(rgn, x, y)`.
pub fn nt_gdi_pt_in_region(c: &mut Call<'_>) -> R64 {
    let (x, y) = (c.a32(1) as i32, c.a32(2) as i32);
    let Some(v) = rects(c, c.a32(0)) else { return Ok(0) };
    Ok(v.iter().any(|r| x >= r.left && x < r.right && y >= r.top && y < r.bottom) as u64)
}

/// `NtGdiGetRegionData(rgn, count, data)`: RGNDATA.
pub fn nt_gdi_get_region_data(c: &mut Call<'_>) -> R64 {
    let (h, count, data) = (c.a32(0), c.a32(1), c.a(2));
    let Some(v) = rects(c, h) else { return Ok(0) };
    let size = 32 + 16 * v.len() as u64;
    if data == 0 {
        return Ok(size);
    }
    if (count as u64) < size {
        return Ok(0);
    }
    c.g.w32(data, 32)?; // dwSize
    c.g.w32(data + 4, 1)?; // RDH_RECTANGLES
    c.g.w32(data + 8, v.len() as u32)?;
    c.g.w32(data + 12, 16 * v.len() as u32)?;
    write_rect(&mut c.g, data + 16, &bounds(&v))?;
    for (i, r) in v.iter().enumerate() {
        write_rect(&mut c.g, data + 32 + 16 * i as u64, r)?;
    }
    Ok(size)
}

/// `NtGdiExtCreateRegion(xform, count, data)`.
pub fn nt_gdi_ext_create_region(c: &mut Call<'_>) -> R64 {
    let (xform, data) = (c.a(0), c.a(2));
    if xform != 0 {
        return Err(c.unimplemented("ExtCreateRegion with a transform"));
    }
    let n = c.g.u32(data + 8)?;
    let mut v = Vec::new();
    for i in 0..n as u64 {
        let r = read_rect(&c.g, data + 32 + 16 * i)?;
        v.push(ordered(r.left, r.top, r.right, r.bottom));
    }
    // The rectangles need not be disjoint.
    let v = v.iter().fold(Vec::new(), |acc, r| union(&acc, &[*r]));
    Ok(handle_ret(alloc(c, Obj::Region(v))?))
}

/// A DC's clip region in device coordinates relative to its origin (`None`:
/// no clipping beyond the visible area).
fn dc_clip(c: &Call<'_>, hdc: u32) -> Option<Option<Vec<Rect>>> {
    match get(c, hdc) {
        Some(Obj::Dc(d)) => Some(d.clip),
        _ => None,
    }
}

fn set_dc_clip(c: &Call<'_>, hdc: u32, clip: Option<Vec<Rect>>) {
    with_obj(c, hdc, |o| {
        if let Obj::Dc(d) = o {
            d.clip = clip;
        }
    });
}

/// The DC's logical-to-device offset (MM_TEXT only).
pub fn dc_offset(c: &mut Call<'_>, hdc: u32) -> R<Option<(i32, i32)>> {
    let Some(Obj::Dc(d)) = get(c, hdc) else { return Ok(None) };
    super::blit::logical_offset(c, d.attr).map(Some)
}

/// What a DC can draw on, in its device coordinates: its visible area
/// intersected with its clip region.
pub fn dc_visible(c: &mut Call<'_>, hdc: u32) -> R<Option<Vec<Rect>>> {
    Ok(super::blit::device_area(c, hdc)?)
}

/// `NtGdiGetAppClipBox(hdc, *rect)`: the clip box in logical coordinates.
pub fn nt_gdi_get_app_clip_box(c: &mut Call<'_>) -> R64 {
    let (hdc, p) = (c.a32(0), c.a(1));
    let Some(vis) = dc_visible(c, hdc)? else { return Ok(ERROR) };
    let Some((dx, dy)) = dc_offset(c, hdc)? else { return Ok(ERROR) };
    let b = bounds(&vis).offset(-dx, -dy);
    write_rect(&mut c.g, p, &b)?;
    Ok(kind(&vis))
}

/// `NtGdiExtSelectClipRgn(hdc, rgn, mode)`: the region is in device
/// coordinates; a null region with RGN_COPY removes clipping.
pub fn nt_gdi_ext_select_clip_rgn(c: &mut Call<'_>) -> R64 {
    let (hdc, rgn, mode) = (c.a32(0), c.a32(1), c.a32(2));
    let Some(cur) = dc_clip(c, hdc) else { return Ok(ERROR) };
    let new = if rgn == 0 {
        if mode != RGN_COPY {
            return Ok(ERROR);
        }
        None
    } else {
        let Some(r) = rects(c, rgn) else { return Ok(ERROR) };
        match cur {
            None => match mode {
                RGN_COPY | RGN_AND => Some(r),
                // No clip region means everything; combining with it needs
                // the device's extent.
                _ => return Err(c.unimplemented(&format!("ExtSelectClipRgn mode {mode} on an unclipped DC"))),
            },
            Some(_) if mode == RGN_COPY => Some(r),
            Some(cur) => match combine(&cur, &r, mode) {
                Some(v) => Some(v),
                None => return Ok(ERROR),
            },
        }
    };
    set_dc_clip(c, hdc, new);
    let vis = dc_visible(c, hdc)?.unwrap_or_default();
    Ok(kind(&vis))
}

/// `NtGdiIntersectClipRect(hdc, left, top, right, bottom)` (logical).
pub fn nt_gdi_intersect_clip_rect(c: &mut Call<'_>) -> R64 {
    clip_rect_op(c, true)
}

/// `NtGdiExcludeClipRect(hdc, left, top, right, bottom)` (logical).
pub fn nt_gdi_exclude_clip_rect(c: &mut Call<'_>) -> R64 {
    clip_rect_op(c, false)
}

fn clip_rect_op(c: &mut Call<'_>, intersect_op: bool) -> R64 {
    let hdc = c.a32(0);
    let r = ordered(c.a32(1) as i32, c.a32(2) as i32, c.a32(3) as i32, c.a32(4) as i32);
    let Some((dx, dy)) = dc_offset(c, hdc)? else { return Ok(ERROR) };
    let r = r.offset(dx, dy);
    let Some(cur) = dc_clip(c, hdc) else { return Ok(ERROR) };
    let cur = match cur {
        Some(v) => v,
        // Unclipped: the whole visible area.
        None => dc_visible(c, hdc)?.unwrap_or_default(),
    };
    let new = if intersect_op { intersect(&cur, &[r]) } else { coalesce(subtract(&cur, &[r])) };
    set_dc_clip(c, hdc, Some(new));
    let vis = dc_visible(c, hdc)?.unwrap_or_default();
    Ok(kind(&vis))
}

/// `NtGdiOffsetClipRgn(hdc, x, y)` (logical units).
pub fn nt_gdi_offset_clip_rgn(c: &mut Call<'_>) -> R64 {
    let (hdc, x, y) = (c.a32(0), c.a32(1) as i32, c.a32(2) as i32);
    let Some(cur) = dc_clip(c, hdc) else { return Ok(ERROR) };
    if let Some(v) = cur {
        set_dc_clip(c, hdc, Some(v.iter().map(|r| r.offset(x, y)).collect()));
    }
    let vis = dc_visible(c, hdc)?.unwrap_or_default();
    Ok(kind(&vis))
}

/// `NtGdiGetRandomRgn(hdc, rgn, which)`: 1 = the clip region (0 when there
/// is none), 4 = SYSRGN, the visible region in screen coordinates.
pub fn nt_gdi_get_random_rgn(c: &mut Call<'_>) -> R64 {
    let (hdc, rgn, which) = (c.a32(0), c.a32(1), c.a32(2));
    let Some(cur) = dc_clip(c, hdc) else { return Ok(u64::MAX) };
    match which {
        1 => match cur {
            None => Ok(0),
            Some(v) => {
                set(c, rgn, v);
                Ok(1)
            }
        },
        4 => {
            let vis = dc_visible(c, hdc)?.unwrap_or_default();
            let (sx, sy) = super::blit::screen_origin(c, hdc)?;
            set(c, rgn, vis.iter().map(|r| r.offset(sx, sy)).collect());
            Ok(1)
        }
        _ => Err(c.unimplemented(&format!("GetRandomRgn({which})"))),
    }
}

/// `NtGdiFillRgn(hdc, rgn, brush)`: the region is in logical coordinates.
pub fn nt_gdi_fill_rgn(c: &mut Call<'_>) -> R64 {
    let (hdc, rgn, brush) = (c.a32(0), c.a32(1), c.a32(2));
    let Some(v) = rects(c, rgn) else { return Ok(0) };
    for r in v {
        super::blit::fill_rect(c, hdc, &r, brush)?;
    }
    Ok(1)
}
