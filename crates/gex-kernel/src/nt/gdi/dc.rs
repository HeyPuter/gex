//! Device contexts: memory DCs, the display DC and window DCs, their
//! `DC_ATTR`s, selected objects and saved states (win32u's dc.c).

use super::super::user::{handle_ret, window::Rect, R64};
use super::{alloc, get, obj, set_user_pointer, stock, with_obj, Obj};
use crate::nt::{off, Call, R};

#[derive(Clone, Debug)]
pub struct Saved {
    attr: Vec<u8>,
    bitmap: u32,
    brush: u32,
    pen: u32,
    font: u32,
    clip: Option<Vec<Rect>>,
}

#[derive(Clone, Debug)]
pub struct Dc {
    /// Guest address of the DC's `DC_ATTR`.
    pub attr: u64,
    pub memory: bool,
    /// For a window DC: the window, and whether it covers the whole window
    /// (GetWindowDC) or the client area.
    pub hwnd: u32,
    pub whole_window: bool,
    pub bitmap: u32,
    pub brush: u32,
    pub pen: u32,
    pub font: u32,
    pub palette: u32,
    /// The clip region (device coordinates, disjoint rectangles), if any.
    pub clip: Option<Vec<Rect>>,
    pub saved: Vec<Saved>,
    /// The pixel format a window DC's window was given (SetPixelFormat).
    pub pixel_format: i32,
}

const DC_ATTR_SLOTS: u64 = 64;

/// A zeroed `DC_ATTR` slot from the pool.
fn alloc_attr(c: &mut Call<'_>) -> R<u64> {
    if let Some(a) = c.p.gdi.lock().unwrap().attr_pool.pop() {
        return Ok(a);
    }
    let size = off!(c.p.abi, dc_attr::SIZEOF);
    let block = crate::nt::sysinfo::alloc_private(c, size * DC_ATTR_SLOTS, "[dc attrs]")?;
    let mut g = c.p.gdi.lock().unwrap();
    for i in 1..DC_ATTR_SLOTS {
        g.attr_pool.push(block + i * size);
    }
    Ok(block)
}

/// Wine's `set_initial_dc_state`.
fn init_attr(c: &mut Call<'_>, attr: u64, hdc: u32, vis: Rect) -> R<()> {
    let abi = c.p.abi;
    macro_rules! a {
        ($f:ident) => {
            attr + off!(abi, dc_attr::$f)
        };
    }
    c.g.zero(attr, off!(abi, dc_attr::SIZEOF) as usize)?;
    c.g.w32(a!(HDC), hdc)?;
    c.g.w32(a!(WND_EXT), 1)?;
    c.g.w32(a!(WND_EXT) + 4, 1)?;
    c.g.w32(a!(VPORT_EXT), 1)?;
    c.g.w32(a!(VPORT_EXT) + 4, 1)?;
    c.g.w32(a!(MITER_LIMIT), 10.0f32.to_bits())?;
    c.g.w16(a!(ROP_MODE), 13)?; // R2_COPYPEN
    c.g.w16(a!(POLY_FILL_MODE), 1)?; // ALTERNATE
    c.g.w16(a!(STRETCH_BLT_MODE), 1)?; // BLACKONWHITE
    c.g.w16(a!(REL_ABS_MODE), 1)?; // ABSOLUTE
    c.g.w16(a!(BACKGROUND_MODE), 2)?; // OPAQUE
    c.g.w32(a!(BACKGROUND_COLOR), 0xffffff)?;
    c.g.w32(a!(BRUSH_COLOR), 0xffffff)?;
    c.g.w32(a!(PEN_COLOR), 0)?;
    c.g.w32(a!(TEXT_COLOR), 0)?;
    c.g.w32(a!(MAP_MODE), 1)?; // MM_TEXT
    c.g.w32(a!(GRAPHICS_MODE), 1)?; // GM_COMPATIBLE
    c.g.w32(a!(ARC_DIRECTION), 1)?; // AD_COUNTERCLOCKWISE
    c.g.w32(a!(VIRTUAL_RES), 1920)?;
    c.g.w32(a!(VIRTUAL_RES) + 4, 1080)?;
    c.g.w32(a!(VIRTUAL_SIZE), 508)?;
    c.g.w32(a!(VIRTUAL_SIZE) + 4, 286)?;
    super::super::user::write_rect(&mut c.g, a!(VIS_RECT), &vis)?;
    Ok(())
}

/// Create a DC object with default objects selected.
pub fn create(c: &mut Call<'_>, memory: bool, hwnd: u32, whole_window: bool) -> R<u32> {
    let attr = alloc_attr(c)?;
    let d = Dc {
        attr,
        memory,
        hwnd,
        whole_window,
        bitmap: if memory { stock(c, stock::DEFAULT_BITMAP) } else { 0 },
        brush: stock(c, stock::WHITE_BRUSH),
        pen: stock(c, stock::BLACK_PEN),
        font: stock(c, stock::SYSTEM_FONT),
        palette: stock(c, stock::DEFAULT_PALETTE),
        clip: None,
        saved: Vec::new(),
        pixel_format: 0,
    };
    let h = alloc(c, Obj::Dc(d))?;
    let vis = if memory { Rect { left: 0, top: 0, right: 1, bottom: 1 } } else { Rect { left: 0, top: 0, right: 1920, bottom: 1080 } };
    init_attr(c, attr, h, vis)?;
    set_user_pointer(c, h, attr)?;
    Ok(h)
}

/// Release a DC's resources (its handle is already gone).
pub fn release(c: &mut Call<'_>, d: Dc) -> R<()> {
    if d.memory {
        if let Some(Obj::Bitmap(_)) = get(c, d.bitmap) {
            with_obj(c, d.bitmap, |o| {
                if let Obj::Bitmap(b) = o {
                    b.selected = false;
                }
            });
        }
    }
    c.p.gdi.lock().unwrap().attr_pool.push(d.attr);
    Ok(())
}

/// `NtGdiCreateCompatibleDC(hdc)`.
pub fn nt_gdi_create_compatible_dc(c: &mut Call<'_>) -> R64 {
    Ok(handle_ret(create(c, true, 0, false)?))
}

/// `NtGdiOpenDCW(device, devmode, output, type, is_display, ...)`: the
/// display is the only device.
pub fn nt_gdi_open_dcw(c: &mut Call<'_>) -> R64 {
    let (device, _devmode, _output, ty, is_display) = (c.a(0), c.a(1), c.a(2), c.a32(3), c.a32(4));
    if is_display == 0 && device != 0 {
        let name = super::super::user::read_name(&c.g, device)?;
        return Err(c.unimplemented(&format!("CreateDC for a printer or other device ({name:?}, type {ty})")));
    }
    Ok(handle_ret(create(c, false, 0, false)?))
}

/// `NtGdiSelectBitmap` / `SelectBrush` / `SelectPen` / `SelectFont`
/// `(hdc, obj)`: the previous object.
pub fn nt_gdi_select(c: &mut Call<'_>, kind: u32) -> R64 {
    let (hdc, h) = (c.a32(0), c.a32(1));
    let Some(Obj::Dc(d)) = get(c, hdc) else { return Ok(0) };
    let Some(o) = get(c, h) else { return Ok(0) };
    let ok = match (kind, &o) {
        (obj::BITMAP, Obj::Bitmap(b)) => {
            if !d.memory || (b.selected && d.bitmap != h) {
                return Ok(0);
            }
            true
        }
        (obj::BRUSH, Obj::Brush(_)) | (obj::PEN, Obj::Pen(_)) | (obj::FONT, Obj::Font(_)) => true,
        _ => false,
    };
    if !ok {
        return Ok(0);
    }
    let full = {
        let g = c.p.gdi.lock().unwrap();
        super::full_handle(&g, h & 0xffff)
    };
    let prev = with_obj(c, hdc, |o| {
        let Obj::Dc(d) = o else { return 0 };
        let slot = match kind {
            obj::BITMAP => &mut d.bitmap,
            obj::BRUSH => &mut d.brush,
            obj::PEN => &mut d.pen,
            _ => &mut d.font,
        };
        std::mem::replace(slot, full)
    })
    .unwrap_or(0);
    if kind == obj::BITMAP {
        if prev != full {
            with_obj(c, prev, |o| {
                if let Obj::Bitmap(b) = o {
                    b.selected = false;
                }
            });
        }
        let (w, h2) = with_obj(c, full, |o| match o {
            Obj::Bitmap(b) => {
                b.selected = true;
                (b.width, b.height)
            }
            _ => (1, 1),
        })
        .unwrap_or((1, 1));
        let vis = off!(c.p.abi, dc_attr::VIS_RECT);
        super::super::user::write_rect(&mut c.g, d.attr + vis, &Rect { left: 0, top: 0, right: w, bottom: h2 })?;
    }
    Ok(handle_ret(prev))
}

/// `NtGdiGetDCObject(hdc, type)`.
pub fn nt_gdi_get_dc_object(c: &mut Call<'_>) -> R64 {
    let (hdc, ty) = (c.a32(0), c.a32(1));
    let Some(Obj::Dc(d)) = get(c, hdc) else { return Ok(0) };
    Ok(handle_ret(match ty {
        obj::PEN | obj::EXTPEN => d.pen,
        obj::BRUSH => d.brush,
        obj::PAL => d.palette,
        obj::FONT => d.font,
        0x05_0000 => d.bitmap, // NTGDI_OBJ_SURF
        _ => return Err(c.unimplemented(&format!("NtGdiGetDCObject type {ty:#x}"))),
    }))
}

/// `NtGdiGetDeviceCaps(hdc, cap)`: the display (1920x1080, 32 bpp, 96 dpi).
pub fn nt_gdi_get_device_caps(c: &mut Call<'_>) -> R64 {
    let (hdc, cap) = (c.a32(0), c.a32(1));
    let Some(Obj::Dc(d)) = get(c, hdc) else { return Ok(0) };
    let mono = d.memory && matches!(get(c, d.bitmap), Some(Obj::Bitmap(b)) if b.bpp == 1) && false;
    let v: i32 = match cap {
        0 => 0x300,         // DRIVERVERSION
        2 => 1,             // TECHNOLOGY: DT_RASDISPLAY
        4 => 508,           // HORZSIZE (mm)
        6 => 286,           // VERTSIZE
        8 => 1920,          // HORZRES
        10 => 1080,         // VERTRES
        12 => if mono { 1 } else { 32 }, // BITSPIXEL
        14 => 1,            // PLANES
        16 => -1,           // NUMBRUSHES
        18 => -1,           // NUMPENS
        20 => 0,            // NUMMARKERS
        22 => 0,            // NUMFONTS
        24 => -1,           // NUMCOLORS
        26 => 0,            // PDEVICESIZE
        28 => 0xff,         // CURVECAPS
        30 => 0xfe,         // LINECAPS
        32 => 0xff,         // POLYGONALCAPS
        34 => 0x7803,       // TEXTCAPS
        36 => 3,            // CLIPCAPS
        38 => 0x7e99,       // RASTERCAPS
        40 => 36,           // ASPECTX
        42 => 36,           // ASPECTY
        44 => 51,           // ASPECTXY
        88 | 90 => 96,      // LOGPIXELSX/Y
        104 => 0,           // SIZEPALETTE
        106 => 20,          // NUMRESERVED
        108 => 0,           // COLORRES
        110..=114 => 0,     // PHYSICALWIDTH.. (printers)
        116 => 60,          // VREFRESH
        117 | 118 => if cap == 117 { 1920 } else { 1080 }, // DESKTOPVERTRES/HORZRES
        119 => 0,           // BLTALIGNMENT
        120 => 0xff,        // SHADEBLENDCAPS
        121 => 0,           // COLORMGMTCAPS
        _ => return Err(c.unimplemented(&format!("GetDeviceCaps index {cap}"))),
    };
    let _ = d;
    Ok(v as u32 as u64)
}

/// `NtGdiGetDCDword(hdc, method, *result)`.
pub fn nt_gdi_get_dc_dword(c: &mut Call<'_>) -> R64 {
    let (hdc, method, out) = (c.a32(0), c.a32(1), c.a(2));
    let Some(Obj::Dc(d)) = get(c, hdc) else { return Ok(0) };
    let abi = c.p.abi;
    macro_rules! a {
        ($f:ident) => {
            d.attr + off!(abi, dc_attr::$f)
        };
    }
    let v = match method {
        0 => c.g.u32(a!(ARC_DIRECTION))?,
        1 => c.g.u32(a!(BACKGROUND_COLOR))?,
        2 => c.g.u16(a!(BACKGROUND_MODE))? as u32,
        3 => c.g.u32(a!(BRUSH_COLOR))?,
        4 => c.g.u32(a!(PEN_COLOR))?,
        5 => c.g.u32(a!(GRAPHICS_MODE))?,
        6 => c.g.u32(a!(LAYOUT))?,
        7 => c.g.u16(a!(POLY_FILL_MODE))? as u32,
        8 => c.g.u16(a!(ROP_MODE))? as u32,
        9 => c.g.u32(a!(TEXT_COLOR))?,
        10 => d.memory as u32,    // NtGdiIsMemDC
        11 => 1,                  // NtGdiHasOpenGL: memory and display DCs alike
        _ => return Err(c.unimplemented(&format!("NtGdiGetDCDword method {method}"))),
    };
    c.g.out32(out, v)?;
    Ok(1)
}

/// `NtGdiGetAndSetDCDword(hdc, method, value, *prev)`.
pub fn nt_gdi_get_and_set_dc_dword(c: &mut Call<'_>) -> R64 {
    let (hdc, method, value, prev_p) = (c.a32(0), c.a32(1), c.a32(2), c.a(3));
    let Some(Obj::Dc(d)) = get(c, hdc) else { return Ok(0) };
    let abi = c.p.abi;
    macro_rules! a {
        ($f:ident) => {
            d.attr + off!(abi, dc_attr::$f)
        };
    }
    let prev = match method {
        8 => {
            // SetMapMode: only MM_TEXT, whose extents are 1:1.
            let prev = c.g.u32(a!(MAP_MODE))?;
            if value != 1 {
                return Err(c.unimplemented(&format!("SetMapMode({value})")));
            }
            c.g.w32(a!(MAP_MODE), 1)?;
            for f in [a!(WND_EXT), a!(WND_EXT) + 4, a!(VPORT_EXT), a!(VPORT_EXT) + 4] {
                c.g.w32(f, 1)?;
            }
            prev
        }
        100 => c.g.u32(a!(BACKGROUND_COLOR)).and_then(|p| c.g.w32(a!(BACKGROUND_COLOR), value).map(|_| p))?,
        101 => c.g.u16(a!(BACKGROUND_MODE)).and_then(|p| c.g.w16(a!(BACKGROUND_MODE), value as u16).map(|_| p as u32))?,
        102 => c.g.u32(a!(TEXT_COLOR)).and_then(|p| c.g.w32(a!(TEXT_COLOR), value).map(|_| p))?,
        103 => c.g.u32(a!(BRUSH_COLOR)).and_then(|p| c.g.w32(a!(BRUSH_COLOR), value).map(|_| p))?,
        104 => c.g.u32(a!(PEN_COLOR)).and_then(|p| c.g.w32(a!(PEN_COLOR), value).map(|_| p))?,
        105 => {
            if value != 1 {
                return Err(c.unimplemented("SetGraphicsMode(GM_ADVANCED)"));
            }
            c.g.u32(a!(GRAPHICS_MODE))?
        }
        106 => c.g.u16(a!(ROP_MODE)).and_then(|p| c.g.w16(a!(ROP_MODE), value as u16).map(|_| p as u32))?,
        107 => c.g.u16(a!(TEXT_ALIGN)).and_then(|p| c.g.w16(a!(TEXT_ALIGN), value as u16).map(|_| p as u32))?,
        _ => return Err(c.unimplemented(&format!("NtGdiGetAndSetDCDword method {method}"))),
    };
    if prev_p == 0 {
        return Ok(0);
    }
    c.g.w32(prev_p, prev)?;
    Ok(1)
}

/// `NtGdiSaveDC(hdc)`: the new save level.
pub fn nt_gdi_save_dc(c: &mut Call<'_>) -> R64 {
    let hdc = c.a32(0);
    let Some(Obj::Dc(d)) = get(c, hdc) else { return Ok(0) };
    let size = off!(c.p.abi, dc_attr::SIZEOF) as usize;
    let attr = c.g.bytes(d.attr, size)?;
    let level = with_obj(c, hdc, |o| {
        let Obj::Dc(d) = o else { return 0 };
        d.saved.push(Saved { attr, bitmap: d.bitmap, brush: d.brush, pen: d.pen, font: d.font, clip: d.clip.clone() });
        d.saved.len()
    })
    .unwrap_or(0);
    let lv = off!(c.p.abi, dc_attr::SAVE_LEVEL);
    c.g.w32(d.attr + lv, level as u32)?;
    Ok(level as u64)
}

/// `NtGdiRestoreDC(hdc, level)`: negative levels count back.
pub fn nt_gdi_restore_dc(c: &mut Call<'_>) -> R64 {
    let (hdc, level) = (c.a32(0), c.a32(1) as i32);
    let Some(Obj::Dc(d)) = get(c, hdc) else { return Ok(0) };
    let n = d.saved.len() as i32;
    let target = if level < 0 { n + level + 1 } else { level };
    if target < 1 || target > n {
        return Ok(0);
    }
    let s = with_obj(c, hdc, |o| {
        let Obj::Dc(d) = o else { return None };
        let s = d.saved[target as usize - 1].clone();
        d.saved.truncate(target as usize - 1);
        d.bitmap = s.bitmap;
        d.brush = s.brush;
        d.pen = s.pen;
        d.font = s.font;
        d.clip = s.clip.clone();
        Some(s)
    })
    .flatten();
    if let Some(s) = s {
        c.g.write(d.attr, &s.attr)?;
        let lv = off!(c.p.abi, dc_attr::SAVE_LEVEL);
        c.g.w32(d.attr + lv, target as u32 - 1)?;
    }
    Ok(1)
}
