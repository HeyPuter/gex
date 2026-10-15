//! GDI text with Windows bitmap fonts (win32u's font.c, as far as raster
//! fonts go): Wine's `.fon` files in the prefix's `C:\windows\Fonts`
//! (`tools/nt-stage-wine.sh` stages them), each an NE executable holding
//! FNT bitmaps at several sizes. A LOGFONT selects a face by name (MS Sans
//! Serif for anything not otherwise named) and a size; text is measured
//! with the FNT's widths and drawn from its bitmaps. TrueType is not
//! rasterized: a program asking for one gets the nearest bitmap face.

use super::super::user::{window::Rect, R64};
use super::{get, Obj};
use crate::nt::{off, Call, Status, R};
use std::sync::OnceLock;

/// One FNT: a face at one size.
pub struct Face {
    pub name: String,
    pub file: &'static str,
    pub height: i32,
    pub ascent: i32,
    pub internal_leading: i32,
    pub external_leading: i32,
    pub avg_width: i32,
    pub max_width: i32,
    pub weight: u16,
    pub italic: u8,
    pub charset: u8,
    pub pitch_and_family: u8,
    pub first: u8,
    pub last: u8,
    pub default_char: u8,
    pub break_char: u8,
    /// Per character from `first`: its width and bitmap (byte columns of
    /// `height` rows each, bit 7 leftmost).
    pub glyphs: Vec<(u16, Vec<u8>)>,
}

impl Face {
    fn glyph(&self, ch: u32) -> &(u16, Vec<u8>) {
        let byte = to_cp1252(ch).unwrap_or(self.first as u32 + self.default_char as u32);
        let i = byte.checked_sub(self.first as u32).filter(|i| (*i as usize) < self.glyphs.len());
        let i = i.unwrap_or(self.default_char as u32).min(self.glyphs.len() as u32 - 1);
        &self.glyphs[i as usize]
    }

    pub fn width(&self, ch: u32) -> i32 {
        self.glyph(ch).0 as i32
    }
}

/// The code page 1252 byte for a character, if it has one (the bitmap
/// fonts here are the Western ones).
fn to_cp1252(ch: u32) -> Option<u32> {
    const HIGH: [u16; 32] = [
        0x20ac, 0, 0x201a, 0x0192, 0x201e, 0x2026, 0x2020, 0x2021, 0x02c6, 0x2030, 0x0160, 0x2039, 0x0152, 0, 0x017d, 0,
        0, 0x2018, 0x2019, 0x201c, 0x201d, 0x2022, 0x2013, 0x2014, 0x02dc, 0x2122, 0x0161, 0x203a, 0x0153, 0, 0x017e, 0x0178,
    ];
    match ch {
        0..=0x7f | 0xa0..=0xff => Some(ch),
        _ => HIGH.iter().position(|&h| h as u32 == ch && h != 0).map(|i| 0x80 + i as u32),
    }
}

fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

/// The FNTs of a `.fon` (an NE executable's RT_FONT resources).
pub fn parse_fon(d: &[u8], file: &'static str) -> Vec<Face> {
    let mut out = Vec::new();
    let Some(ne) = u32_at(d, 0x3c).map(|v| v as usize) else { return out };
    if d.get(ne..ne + 2) != Some(b"NE") {
        return out;
    }
    let Some(rt) = u16_at(d, ne + 0x24).map(|v| ne + v as usize) else { return out };
    let Some(shift) = u16_at(d, rt) else { return out };
    let mut at = rt + 2;
    loop {
        let Some(ty) = u16_at(d, at) else { break };
        if ty == 0 {
            break;
        }
        let count = u16_at(d, at + 2).unwrap_or(0) as usize;
        for i in 0..count {
            let e = at + 8 + i * 12;
            if ty == 0x8008 {
                let (Some(o), Some(l)) = (u16_at(d, e), u16_at(d, e + 2)) else { continue };
                let (o, l) = ((o as usize) << shift, (l as usize) << shift);
                if let Some(face) = d.get(o..(o + l).min(d.len())).and_then(|f| parse_fnt(f, file)) {
                    out.push(face);
                }
            }
        }
        at += 8 + count * 12;
    }
    out
}

/// One FNT (versions 2 and 3).
fn parse_fnt(f: &[u8], file: &'static str) -> Option<Face> {
    let version = u16_at(f, 0)?;
    let b = |at: usize| f.get(at).copied();
    let height = u16_at(f, 88)? as i32;
    let (first, last) = (b(95)?, b(96)?);
    let face_at = u32_at(f, 105)? as usize;
    let name = f.get(face_at..)?.split(|&c| c == 0).next()?.iter().map(|&c| c as char).collect();
    let (table, entry) = match version {
        0x200 => (118, 4),
        0x300 => (148, 6),
        _ => return None,
    };
    let mut glyphs = Vec::new();
    for i in 0..=(last as usize - first as usize) {
        let e = table + i * entry;
        let w = u16_at(f, e)?;
        let o = if entry == 4 { u16_at(f, e + 2)? as usize } else { u32_at(f, e + 2)? as usize };
        let bytes = (w as usize).div_ceil(8) * height as usize;
        glyphs.push((w, f.get(o..o + bytes)?.to_vec()));
    }
    Some(Face {
        name,
        file,
        height,
        ascent: u16_at(f, 74)? as i32,
        internal_leading: u16_at(f, 76)? as i32,
        external_leading: u16_at(f, 78)? as i32,
        avg_width: u16_at(f, 91)? as i32,
        max_width: u16_at(f, 93)? as i32,
        weight: u16_at(f, 83)?,
        italic: b(80)?,
        charset: b(85)?,
        pitch_and_family: b(90)?,
        first,
        last,
        default_char: b(97)?,
        break_char: b(98)?,
        glyphs,
    })
}

/// The fonts drawn with, by family: MS Sans Serif, System, Courier, Small
/// Fonts.
const FILES: &[&str] = &["sserife.fon", "vgasys.fon", "coure.fon", "smalle.fon"];

static FACES: OnceLock<Vec<Face>> = OnceLock::new();

fn faces(c: &Call<'_>) -> &'static [Face] {
    FACES.get_or_init(|| {
        let k = c.kernel();
        let mut all = Vec::new();
        for f in FILES {
            let path = format!("{}/drive_c/windows/Fonts/{f}", c.p.cfg.prefix);
            if let Ok(d) = crate::nt::file::read_guest_file(&k.vfs, &path) {
                all.extend(parse_fon(&d, f));
            }
        }
        if all.is_empty() {
            crate::nt_log!("gex-nt: no bitmap fonts in {}/drive_c/windows/Fonts (tools/nt-stage-wine.sh stages them); text is not drawn", c.p.cfg.prefix);
        }
        all
    })
}

/// The face a DC's font selects: by LOGFONTW's face name and height.
fn face_for(c: &Call<'_>, hdc: u32) -> Option<&'static Face> {
    let all = faces(c);
    if all.is_empty() {
        return None;
    }
    let lf = match get(c, hdc) {
        Some(Obj::Dc(d)) => match get(c, d.font) {
            Some(Obj::Font(lf)) => lf,
            _ => Vec::new(),
        },
        _ => return None,
    };
    let height = lf.get(0..4).map(|b| i32::from_le_bytes(b.try_into().unwrap())).unwrap_or(0);
    let name: String = lf
        .get(28..92)
        .unwrap_or(&[])
        .chunks_exact(2)
        .map(|w| u16::from_le_bytes([w[0], w[1]]))
        .take_while(|&w| w != 0)
        .map(|w| char::from_u32(w as u32).unwrap_or('?'))
        .collect::<String>()
        .to_ascii_lowercase();
    let pitch = lf.get(27).copied().unwrap_or(0) & 3;
    let file = if name == "system" || name == "fixedsys" {
        "vgasys.fon"
    } else if name.starts_with("courier") || name.contains("mono") || pitch == 1 {
        "coure.fon"
    } else if name == "small fonts" {
        "smalle.fon"
    } else {
        "sserife.fon"
    };
    let family: Vec<&Face> = all.iter().filter(|f| f.file == file).collect();
    let family = if family.is_empty() { all.iter().collect() } else { family };
    // A positive height is the cell's, a negative one the character's (the
    // cell less its internal leading); zero is the default, 8 points.
    let want = |f: &Face| match height {
        0 => (f.height - 13).abs(),
        h if h > 0 => (f.height - h).abs(),
        h => (f.height - f.internal_leading + h).abs(),
    };
    family.into_iter().min_by_key(|f| want(f))
}

fn wide(c: &Call<'_>, p: u64, n: usize) -> R<Vec<u32>> {
    if n == 0 || p == 0 {
        return Ok(Vec::new());
    }
    Ok(c.g.wchars(p, n)?.into_iter().map(|w| w as u32).collect())
}

/// `NtGdiGetTextExtentExW(hdc, str, count, max_ext, *nfit, *dxs, *size,
/// flags)`: the string's size, and how many characters fit in `max_ext`
/// and where each ends.
pub fn nt_gdi_get_text_extent_ex_w(c: &mut Call<'_>) -> R64 {
    let (hdc, str_, count, max_ext, nfit, dxs, size) =
        (c.a32(0), c.a(1), c.a32(2) as i32, c.a32(3) as i32, c.a(4), c.a(5), c.a(6));
    if count < 0 {
        return Ok(0);
    }
    let Some(face) = face_for(c, hdc) else { return Ok(0) };
    let text = wide(c, str_, count as usize)?;
    let mut x = 0i32;
    let mut fit = 0;
    for (i, &ch) in text.iter().enumerate() {
        x += face.width(ch);
        if nfit != 0 && x > max_ext && fit == i {
            // `fit` stops at the first character past the limit.
        } else if x <= max_ext || nfit == 0 {
            fit = i + 1;
        }
        if dxs != 0 {
            c.g.w32(dxs + 4 * i as u64, x as u32)?;
        }
    }
    if nfit != 0 {
        c.g.w32(nfit, fit as u32)?;
    }
    c.g.w32(size, x as u32)?;
    c.g.w32(size + 4, face.height as u32)?;
    Ok(1)
}

/// `NtGdiGetTextMetricsW(hdc, TEXTMETRICW *, flags)`.
pub fn nt_gdi_get_text_metrics_w(c: &mut Call<'_>) -> R64 {
    let (hdc, tm) = (c.a32(0), c.a(1));
    let Some(f) = face_for(c, hdc) else { return Ok(0) };
    let longs = [
        f.height,
        f.ascent,
        f.height - f.ascent,
        f.internal_leading,
        f.external_leading,
        f.avg_width,
        f.max_width,
        f.weight as i32,
        0,  // overhang
        96, // digitized aspect x
        96, // y
    ];
    for (i, v) in longs.iter().enumerate() {
        c.g.w32(tm + 4 * i as u64, *v as u32)?;
    }
    let chars = [f.first as u16, f.last as u16, f.first as u16 + f.default_char as u16, f.first as u16 + f.break_char as u16];
    for (i, v) in chars.iter().enumerate() {
        c.g.w16(tm + 44 + 2 * i as u64, *v)?;
    }
    // Italic, underlined, struck out, pitch and family (TMPF_FIXED_PITCH
    // set means variable, as in the FNT), charset.
    let bytes = [f.italic, 0, 0, f.pitch_and_family, f.charset];
    for (i, v) in bytes.iter().enumerate() {
        c.g.w8(tm + 52 + i as u64, *v)?;
    }
    Ok(1)
}

/// `NtGdiGetTextFaceW(hdc, count, name, alias)`: the face name's length with
/// its terminator, copied when `name` is given.
pub fn nt_gdi_get_text_face_w(c: &mut Call<'_>) -> R64 {
    let (hdc, count, name) = (c.a32(0), c.a32(1) as i32, c.a(2));
    let Some(f) = face_for(c, hdc) else { return Ok(0) };
    let mut w: Vec<u16> = f.name.encode_utf16().collect();
    w.push(0);
    if name == 0 {
        return Ok(w.len() as u64);
    }
    let n = (count.max(0) as usize).min(w.len());
    if n == 0 {
        return Ok(0);
    }
    w.truncate(n);
    w[n - 1] = 0;
    c.g.write_wchars(name, &w)?;
    Ok(n as u64)
}

/// `NtGdiGetCharWidthW(hdc, first, last, chars, flags, buf)`: widths, as
/// INTs with NTGDI_GETCHARWIDTH_INT (2), else floats; with `chars`, `last`
/// is their count.
pub fn nt_gdi_get_char_width_w(c: &mut Call<'_>) -> R64 {
    let (hdc, first, last, chars, flags, buf) = (c.a32(0), c.a32(1), c.a32(2), c.a(3), c.a32(4), c.a(5));
    let Some(f) = face_for(c, hdc) else { return Ok(0) };
    let list: Vec<u32> = if chars != 0 { wide(c, chars, last as usize)? } else { (first..=last).collect() };
    for (i, ch) in list.iter().enumerate() {
        let w = f.width(*ch);
        // NTGDI_GETCHARWIDTH_INT (2) asks for ints; otherwise floats.
        let v = if flags & 2 != 0 { w as u32 } else { (w as f32).to_bits() };
        c.g.w32(buf + 4 * i as u64, v)?;
    }
    Ok(1)
}

/// `NtGdiGetCharABCWidthsW(hdc, first, last, chars, flags, buf)`: a bitmap
/// font's glyphs fill their cells: A and C are zero.
pub fn nt_gdi_get_char_abc_widths_w(c: &mut Call<'_>) -> R64 {
    let (hdc, first, last, chars, flags, buf) = (c.a32(0), c.a32(1), c.a32(2), c.a(3), c.a32(4), c.a(5));
    let Some(f) = face_for(c, hdc) else { return Ok(0) };
    let list: Vec<u32> = if chars != 0 { wide(c, chars, last as usize)? } else { (first..=last).collect() };
    for (i, ch) in list.iter().enumerate() {
        let w = f.width(*ch);
        let at = buf + 12 * i as u64;
        // NTGDI_GETCHARABCWIDTHS_INT (1): ABC; else ABCFLOAT.
        let (a, b, cc) = if flags & 1 != 0 { (0u32, w as u32, 0u32) } else { (0f32.to_bits(), (w as f32).to_bits(), 0f32.to_bits()) };
        c.g.w32(at, a)?;
        c.g.w32(at + 4, b)?;
        c.g.w32(at + 8, cc)?;
    }
    Ok(1)
}

/// `NtGdiGetGlyphIndicesW(hdc, str, count, indices, flags)`: a bitmap font's
/// glyph index is its character code; one it lacks is 0xffff with
/// GGI_MARK_NONEXISTING_GLYPHS (1), else the default character.
pub fn nt_gdi_get_glyph_indices_w(c: &mut Call<'_>) -> R64 {
    let (hdc, str_, count, out, flags) = (c.a32(0), c.a(1), c.a32(2) as i32, c.a(3), c.a32(4));
    let Some(f) = face_for(c, hdc) else { return Ok(0xffff_ffff) };
    let text = wide(c, str_, count.max(0) as usize)?;
    for (i, ch) in text.iter().enumerate() {
        let has = to_cp1252(*ch).is_some_and(|b| b >= f.first as u32 && b <= f.last as u32);
        let v = if has { to_cp1252(*ch).unwrap() as u16 } else if flags & 1 != 0 { 0xffff } else { f.first as u16 + f.default_char as u16 };
        c.g.w16(out + 2 * i as u64, v)?;
    }
    Ok(text.len() as u64)
}

/// `NtGdiExtTextOutW(hdc, x, y, flags, rect, str, count, dx, cp)`.
pub fn nt_gdi_ext_text_out_w(c: &mut Call<'_>) -> R64 {
    const ETO_OPAQUE: u32 = 2;
    const ETO_CLIPPED: u32 = 4;
    const ETO_GLYPH_INDEX: u32 = 0x10;
    let (hdc, x, y, flags, rect_p, str_, count, dx_p) =
        (c.a32(0), c.a32(1) as i32, c.a32(2) as i32, c.a32(3), c.a(4), c.a(5), c.a32(6), c.a(7));
    let rect = if rect_p != 0 { Some(super::super::user::read_rect(&c.g, rect_p)?) } else { None };
    let Some(Obj::Dc(d)) = get(c, hdc) else { return Ok(0) };
    let abi = c.p.abi;
    let align = c.g.u16(d.attr + off!(abi, dc_attr::TEXT_ALIGN))? as u32;
    let bk_mode = c.g.u16(d.attr + off!(abi, dc_attr::BACKGROUND_MODE))?;
    let text_rgb = super::blit::colorref_to_rgb(c.g.u32(d.attr + off!(abi, dc_attr::TEXT_COLOR))?);
    let bk_rgb = super::blit::colorref_to_rgb(c.g.u32(d.attr + off!(abi, dc_attr::BACKGROUND_COLOR))?);
    if let (Some(r), true) = (rect, flags & ETO_OPAQUE != 0) {
        super::blit::fill_solid(c, hdc, &r, bk_rgb, None)?;
    }
    let Some(f) = face_for(c, hdc) else { return Ok(1) };
    // A glyph index is the character code (nt_gdi_get_glyph_indices_w).
    let text = wide(c, str_, count as usize)?;
    let _ = ETO_GLYPH_INDEX;
    // Advances: the caller's, or the font's.
    let mut adv = Vec::with_capacity(text.len());
    for (i, ch) in text.iter().enumerate() {
        adv.push(if dx_p != 0 { c.g.u32(dx_p + 4 * i as u64)? as i32 } else { f.width(*ch) });
    }
    let total: i32 = adv.iter().sum();
    // TA_RIGHT 2, TA_CENTER 6; TA_BOTTOM 8, TA_BASELINE 24.
    let left = match align & 6 {
        2 => x - total,
        6 => x - total / 2,
        _ => x,
    };
    let top = match align & 24 {
        8 => y - f.height,
        24 => y - f.ascent,
        _ => y,
    };
    let clip = if flags & ETO_CLIPPED != 0 { rect } else { None };
    if bk_mode == 2 && flags & ETO_OPAQUE == 0 {
        let cell = Rect { left, top, right: left + total, bottom: top + f.height };
        super::blit::fill_solid(c, hdc, &cell, bk_rgb, clip.as_ref())?;
    }
    let mut points = Vec::new();
    let mut pen = left;
    for (i, ch) in text.iter().enumerate() {
        let (w, bits) = f.glyph(*ch);
        let h = f.height as usize;
        for col in 0..*w as usize {
            let byte_col = col / 8;
            let bit = 7 - (col % 8);
            for row in 0..h {
                if bits.get(byte_col * h + row).is_some_and(|b| b >> bit & 1 != 0) {
                    points.push((pen + col as i32, top + row as i32));
                }
            }
        }
        pen += adv[i];
    }
    super::blit::plot(c, hdc, &points, text_rgb, clip.as_ref())?;
    Ok(1)
}

#[allow(dead_code)]
fn _unused(_: Status) {}
