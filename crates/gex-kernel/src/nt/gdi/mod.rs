//! GDI: what win32u's unix side does for Wine's PE `gdi32.dll` (objects,
//! DCs, bitmaps, blits), with the shared handle table and `DC_ATTR`s gdi32
//! reads directly (docs/nt-layer.md §4, §6a).
//!
//! Handles live in `GDI_SHARED_MEMORY` in guest memory, as Wine lays it out:
//! entry `idx` of `peb->GdiSharedHandleTable`, handle `idx | Unique << 16`,
//! where `Unique` is the type (7 bits), the stock flag and a generation byte.
//! A DC's `DC_ATTR` is guest memory too; gdi32 reads and writes colors,
//! modes and origins there itself, so the DC state that drawing uses is read
//! back from it.
//!
//! Pixels are guest memory as well: every bitmap's bits are allocated from
//! the NT allocator, so a DIB section's bits pointer is the bitmap itself and
//! a window surface (§6a) is just another bitmap.

pub mod bitmap;
pub mod blit;
pub mod d3dkmt;
pub mod dc;
pub mod font;
pub mod region;
pub mod surface;

use super::user::{handle_ret, window::Rect, R64};
use super::{off, st, sysno, Call, Status, R};
use std::collections::HashMap;

pub const MAX_HANDLES: u32 = 0x10000;
pub const FIRST_HANDLE: u32 = 32;

/// `NTGDI_OBJ_*` (the type bits of a handle).
pub mod obj {
    pub const DC: u32 = 0x01_0000;
    pub const REGION: u32 = 0x04_0000;
    pub const PAL: u32 = 0x08_0000;
    pub const BITMAP: u32 = 0x09_0000;
    pub const FONT: u32 = 0x0a_0000;
    pub const BRUSH: u32 = 0x10_0000;
    pub const PEN: u32 = 0x30_0000;
    pub const EXTPEN: u32 = 0x50_0000;
    pub const MEMDC: u32 = 0x41_0000;
}

/// Stock object indices (wingdi.h), which are slots `FIRST_HANDLE + n`.
pub mod stock {
    pub const WHITE_BRUSH: u32 = 0;
    pub const BLACK_BRUSH: u32 = 4;
    pub const NULL_BRUSH: u32 = 5;
    pub const WHITE_PEN: u32 = 6;
    pub const BLACK_PEN: u32 = 7;
    pub const SYSTEM_FONT: u32 = 13;
    pub const DEFAULT_PALETTE: u32 = 15;
    pub const DC_BRUSH: u32 = 18;
    pub const DC_PEN: u32 = 19;
    pub const DEFAULT_BITMAP: u32 = 20;
}

#[derive(Clone, Debug)]
pub struct Brush {
    pub style: u32,
    pub color: u32,
    pub hatch: u64,
    /// A pattern brush's bitmap (a private copy).
    pub pattern: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct Pen {
    pub style: u32,
    pub width: u32,
    pub color: u32,
}

#[derive(Clone, Debug)]
pub enum Obj {
    Dc(dc::Dc),
    Bitmap(bitmap::Bitmap),
    Brush(Brush),
    Pen(Pen),
    /// A LOGFONTW, kept for GetObject.
    Font(Vec<u8>),
    Palette(Vec<u32>),
    Region(Vec<Rect>),
}

impl Obj {
    pub fn kind(&self) -> u32 {
        match self {
            Obj::Dc(d) => {
                if d.memory {
                    obj::MEMDC
                } else {
                    obj::DC
                }
            }
            Obj::Bitmap(_) => obj::BITMAP,
            Obj::Brush(_) => obj::BRUSH,
            Obj::Pen(_) => obj::PEN,
            Obj::Font(_) => obj::FONT,
            Obj::Palette(_) => obj::PAL,
            Obj::Region(_) => obj::REGION,
        }
    }
}

pub struct Gdi {
    /// Guest address of `GDI_SHARED_MEMORY`, 0 until initialized.
    pub table: u64,
    pub objects: HashMap<u32, Obj>,
    generations: Vec<u8>,
    next_unused: u32,
    free: Vec<u32>,
    /// DC_ATTR slots: guest memory handed out one DC at a time.
    pub attr_pool: Vec<u64>,
    /// GetSysColorBrush's brushes, made on first use; like stock objects
    /// they cannot be deleted.
    pub sys_brushes: HashMap<u32, u32>,
}

impl Default for Gdi {
    fn default() -> Self {
        Gdi {
            table: 0,
            objects: HashMap::new(),
            generations: vec![0; MAX_HANDLES as usize],
            next_unused: FIRST_HANDLE,
            free: Vec::new(),
            attr_pool: Vec::new(),
            sys_brushes: HashMap::new(),
        }
    }
}

/// The handle table entry for index `idx`.
fn entry(table: u64, idx: u32) -> u64 {
    table + idx as u64 * 0x18
}

/// A handle's table index, if it names a live object.
pub fn index(g: &Gdi, h: u32) -> Option<u32> {
    let idx = h & 0xffff;
    if idx >= MAX_HANDLES || !g.objects.contains_key(&idx) {
        return None;
    }
    let hi = h >> 16;
    let unique = unique(g, idx);
    (hi == 0 || hi == unique as u32).then_some(idx)
}

fn unique(g: &Gdi, idx: u32) -> u16 {
    let kind = g.objects[&idx].kind();
    let stock = idx < FIRST_HANDLE + 24;
    ((kind >> 16) & 0x7f) as u16 | (stock as u16) << 7 | (g.generations[idx as usize] as u16) << 8
}

pub fn full_handle(g: &Gdi, idx: u32) -> u32 {
    idx | (unique(g, idx) as u32) << 16
}

/// Allocate a handle for `o` and publish it in the shared table.
pub fn alloc(c: &mut Call<'_>, o: Obj) -> R<u32> {
    let kind = o.kind();
    let pid = c.p.pid;
    let (idx, table, h) = {
        let mut g = c.p.gdi.lock().unwrap();
        let idx = match g.free.pop() {
            Some(i) => i,
            None => {
                if g.next_unused >= MAX_HANDLES {
                    return Err(Status(st::NO_MEMORY));
                }
                g.next_unused += 1;
                g.next_unused - 1
            }
        };
        let gen = &mut g.generations[idx as usize];
        *gen = if *gen >= 0x7f { 1 } else { *gen + 1 };
        g.objects.insert(idx, o);
        (idx, g.table, full_handle(&g, idx))
    };
    let e = entry(table, idx);
    let abi = c.p.abi;
    c.g.zero(e, 0x18)?;
    c.g.w64(e + off!(abi, gdi_handle_entry::OBJECT), 1)?;
    c.g.w32(e + off!(abi, gdi_handle_entry::OWNER), pid & 0xffff)?;
    c.g.w16(e + off!(abi, gdi_handle_entry::UNIQUE), (h >> 16) as u16)?;
    c.g.w8(e + off!(abi, gdi_handle_entry::TYPE), ((kind >> 16) & 0x1f) as u8)?;
    Ok(h)
}

/// Set a handle's `UserPointer` (a DC's `DC_ATTR`).
pub fn set_user_pointer(c: &mut Call<'_>, h: u32, p: u64) -> R<()> {
    let table = c.p.gdi.lock().unwrap().table;
    c.g.w64(entry(table, h & 0xffff) + off!(c.p.abi, gdi_handle_entry::USERPOINTER), p)
}

/// Free a handle (the object must be dropped by the caller).
pub fn free(c: &mut Call<'_>, h: u32) -> R<Option<Obj>> {
    let (o, table) = {
        let mut g = c.p.gdi.lock().unwrap();
        let Some(idx) = index(&g, h) else { return Ok(None) };
        if idx < FIRST_HANDLE + 24 {
            return Ok(None); // stock objects are never freed
        }
        let o = g.objects.remove(&idx);
        g.free.push(idx);
        (o, g.table)
    };
    c.g.zero(entry(table, h & 0xffff), 0x18)?;
    Ok(o)
}

/// The object a handle names (a clone; objects are small or hold only
/// guest addresses).
pub fn get(c: &Call<'_>, h: u32) -> Option<Obj> {
    let g = c.p.gdi.lock().unwrap();
    index(&g, h).and_then(|i| g.objects.get(&i).cloned())
}

pub fn with_obj<T>(c: &Call<'_>, h: u32, f: impl FnOnce(&mut Obj) -> T) -> Option<T> {
    let mut g = c.p.gdi.lock().unwrap();
    let i = index(&g, h)?;
    g.objects.get_mut(&i).map(f)
}

/// The stock object `n`'s full handle.
pub fn stock(c: &Call<'_>, n: u32) -> u32 {
    let g = c.p.gdi.lock().unwrap();
    full_handle(&g, FIRST_HANDLE + n)
}

/// Create the shared table and the stock objects, in Wine's slot order
/// (`init_stock_objects`), the first time GDI is used.
pub fn ensure_init(c: &mut Call<'_>) -> R<()> {
    if c.p.gdi.lock().unwrap().table != 0 {
        return Ok(());
    }
    let size = MAX_HANDLES as u64 * 0x18;
    let table = super::sysinfo::alloc_private(c, size, "[gdi handles]")?;
    c.p.gdi.lock().unwrap().table = table;
    let peb = c.p.peb.load(std::sync::atomic::Ordering::Relaxed);
    c.g.wptr(peb + off!(c.p.abi, peb::GDISHAREDHANDLETABLE), table)?;
    let brush = |style: u32, color: u32| Obj::Brush(Brush { style, color, hatch: 0, pattern: None });
    let pen = |style: u32, color: u32| Obj::Pen(Pen { style, width: 0, color });
    let font = || Obj::Font(vec![0u8; 92]);
    let objs = vec![
        brush(0, 0xffffff),
        brush(0, 0xc0c0c0),
        brush(0, 0x808080),
        brush(0, 0x404040),
        brush(0, 0),
        brush(1, 0), // BS_NULL
        pen(0, 0xffffff),
        pen(0, 0),
        pen(5, 0), // PS_NULL
        font(),
        font(),
        font(),
        font(),
        font(),
        font(),
        Obj::Palette(bitmap::DEFAULT_PALETTE.to_vec()),
        font(),
        font(),
        brush(0, 0xffffff), // DC_BRUSH
        pen(0, 0),          // DC_PEN
    ];
    for o in objs {
        alloc(c, o)?;
    }
    let bm = bitmap::Bitmap::new_ddb(c, 1, 1, 1)?;
    let h = alloc(c, Obj::Bitmap(bm))?;
    debug_assert_eq!(h & 0xffff, FIRST_HANDLE + stock::DEFAULT_BITMAP);
    for _ in 0..3 {
        alloc(c, font())?;
    }
    // Stock objects carry the stock flag in their handle.
    let table = c.p.gdi.lock().unwrap().table;
    for i in FIRST_HANDLE..FIRST_HANDLE + 24 {
        let u = {
            let g = c.p.gdi.lock().unwrap();
            unique(&g, i)
        };
        c.g.w16(entry(table, i) + off!(c.p.abi, gdi_handle_entry::UNIQUE), u)?;
    }
    Ok(())
}

/// Handle one `NtGdi*` syscall.
pub fn dispatch(c: &mut Call<'_>) -> R64 {
    use sysno::win32u as n;
    ensure_init(c)?;
    match c.nr {
        n::NtGdiCreateBitmap => bitmap::nt_gdi_create_bitmap(c),
        n::NtGdiCreateCompatibleBitmap => bitmap::nt_gdi_create_compatible_bitmap(c),
        n::NtGdiCreateDIBSection => bitmap::nt_gdi_create_dib_section(c),
        n::NtGdiGetDIBitsInternal => bitmap::nt_gdi_get_dibits_internal(c),
        n::NtGdiGetBitmapBits => bitmap::nt_gdi_get_bitmap_bits(c),
        n::NtGdiSetBitmapBits => bitmap::nt_gdi_set_bitmap_bits(c),
        n::NtGdiCreatePatternBrushInternal => nt_gdi_create_pattern_brush_internal(c),
        n::NtGdiCreateSolidBrush => nt_gdi_create_solid_brush(c),
        n::NtGdiCreatePen => nt_gdi_create_pen(c),
        n::NtGdiHfontCreate => nt_gdi_hfont_create(c),
        n::NtGdiDeleteObjectApp => nt_gdi_delete_object_app(c),
        n::NtGdiExtGetObjectW => nt_gdi_ext_get_object_w(c),
        n::NtGdiCreateCompatibleDC => dc::nt_gdi_create_compatible_dc(c),
        n::NtGdiOpenDCW => dc::nt_gdi_open_dcw(c),
        n::NtGdiSelectBitmap => dc::nt_gdi_select(c, obj::BITMAP),
        n::NtGdiSelectBrush => dc::nt_gdi_select(c, obj::BRUSH),
        n::NtGdiSelectPen => dc::nt_gdi_select(c, obj::PEN),
        n::NtGdiSelectFont => dc::nt_gdi_select(c, obj::FONT),
        n::NtGdiGetDCObject => dc::nt_gdi_get_dc_object(c),
        n::NtGdiGetDeviceCaps => dc::nt_gdi_get_device_caps(c),
        n::NtGdiGetDCDword => dc::nt_gdi_get_dc_dword(c),
        n::NtGdiGetAndSetDCDword => dc::nt_gdi_get_and_set_dc_dword(c),
        n::NtGdiSaveDC => dc::nt_gdi_save_dc(c),
        n::NtGdiRestoreDC => dc::nt_gdi_restore_dc(c),
        n::NtGdiStretchDIBitsInternal => blit::nt_gdi_stretch_dibits_internal(c),
        n::NtGdiSetDIBitsToDeviceInternal => blit::nt_gdi_set_dibits_to_device_internal(c),
        n::NtGdiBitBlt => blit::nt_gdi_bit_blt(c),
        n::NtGdiStretchBlt => blit::nt_gdi_stretch_blt(c),
        n::NtGdiPatBlt => blit::nt_gdi_pat_blt(c),
        n::NtGdiGetPixel => blit::nt_gdi_get_pixel(c),
        n::NtGdiSetPixel => blit::nt_gdi_set_pixel(c),
        n::NtGdiFlush => Ok(1),
        // Transforms are read from the DC_ATTR at each draw.
        n::NtGdiComputeXformCoefficients => Ok(1),
        n::NtGdiCreateRectRgn => region::nt_gdi_create_rect_rgn(c),
        n::NtGdiSetRectRgn => region::nt_gdi_set_rect_rgn(c),
        n::NtGdiGetRgnBox => region::nt_gdi_get_rgn_box(c),
        n::NtGdiCombineRgn => region::nt_gdi_combine_rgn(c),
        n::NtGdiOffsetRgn => region::nt_gdi_offset_rgn(c),
        n::NtGdiEqualRgn => region::nt_gdi_equal_rgn(c),
        n::NtGdiPtInRegion => region::nt_gdi_pt_in_region(c),
        n::NtGdiGetRegionData => region::nt_gdi_get_region_data(c),
        n::NtGdiExtCreateRegion => region::nt_gdi_ext_create_region(c),
        n::NtGdiGetAppClipBox => region::nt_gdi_get_app_clip_box(c),
        n::NtGdiExtSelectClipRgn => region::nt_gdi_ext_select_clip_rgn(c),
        n::NtGdiIntersectClipRect => region::nt_gdi_intersect_clip_rect(c),
        n::NtGdiExcludeClipRect => region::nt_gdi_exclude_clip_rect(c),
        n::NtGdiOffsetClipRgn => region::nt_gdi_offset_clip_rgn(c),
        n::NtGdiGetRandomRgn => region::nt_gdi_get_random_rgn(c),
        n::NtGdiFillRgn => region::nt_gdi_fill_rgn(c),
        n::NtGdiGetTextExtentExW => font::nt_gdi_get_text_extent_ex_w(c),
        n::NtGdiGetTextMetricsW => font::nt_gdi_get_text_metrics_w(c),
        n::NtGdiGetTextFaceW => font::nt_gdi_get_text_face_w(c),
        n::NtGdiGetCharWidthW => font::nt_gdi_get_char_width_w(c),
        n::NtGdiGetCharABCWidthsW => font::nt_gdi_get_char_abc_widths_w(c),
        n::NtGdiGetGlyphIndicesW => font::nt_gdi_get_glyph_indices_w(c),
        n::NtGdiExtTextOutW => font::nt_gdi_ext_text_out_w(c),
        // The display's gamma ramp: the identity, three 256-entry WORD
        // tables. Setting one is accepted and changes nothing shown.
        n::NtGdiGetDeviceGammaRamp => {
            let ramp = c.a(1);
            for ch in 0..3u64 {
                for i in 0..256u64 {
                    c.g.w16(ramp + 2 * (ch * 256 + i), (i * 257) as u16)?;
                }
            }
            Ok(1)
        }
        n::NtGdiSetDeviceGammaRamp => Ok(1),
        n::NtGdiDdDDIOpenAdapterFromLuid => d3dkmt::open_adapter_from_luid(c),
        n::NtGdiDdDDICreateDevice => d3dkmt::create_device(c),
        n::NtGdiDdDDIQueryVideoMemoryInfo => d3dkmt::query_video_memory_info(c),
        n::NtGdiDdDDICreateDCFromMemory => d3dkmt::create_dc_from_memory(c),
        n::NtGdiDdDDIDestroyDCFromMemory => d3dkmt::destroy_dc_from_memory(c),
        n::NtGdiDdDDIEscape => d3dkmt::escape(c),
        n::NtGdiDdDDICloseAdapter
        | n::NtGdiDdDDIDestroyDevice
        | n::NtGdiDdDDISetQueuedLimit
        | n::NtGdiDdDDICheckVidPnExclusiveOwnership => d3dkmt::check_adapter(c),
        nr => Err(c.unimplemented(&format!("win32u {} ({nr:#x})", super::syscall_name(nr)))),
    }
}

/// GetSysColorBrush(index): a solid brush of the system color, kept.
pub fn sys_color_brush(c: &mut Call<'_>, index: u32) -> R<u32> {
    ensure_init(c)?;
    if index > 30 {
        return Ok(0);
    }
    if let Some(&h) = c.p.gdi.lock().unwrap().sys_brushes.get(&index) {
        return Ok(h);
    }
    let color = super::user::sysparams::sys_color(index as i32);
    let h = alloc(c, Obj::Brush(Brush { style: 0, color, hatch: 0, pattern: None }))?;
    c.p.gdi.lock().unwrap().sys_brushes.insert(index, h);
    Ok(h)
}

/// `NtGdiCreateSolidBrush(color, brush)`.
fn nt_gdi_create_solid_brush(c: &mut Call<'_>) -> R64 {
    let color = c.a32(0);
    Ok(handle_ret(alloc(c, Obj::Brush(Brush { style: 0, color, hatch: 0, pattern: None }))?))
}

/// `NtGdiCreatePatternBrushInternal(bitmap, pen, is_8x8)`: the brush keeps
/// a copy of the bitmap.
fn nt_gdi_create_pattern_brush_internal(c: &mut Call<'_>) -> R64 {
    let hbm = c.a32(0);
    let Some(Obj::Bitmap(bm)) = get(c, hbm) else { return Ok(0) };
    let copy = bm.duplicate(c)?;
    let ph = alloc(c, Obj::Bitmap(copy))?;
    Ok(handle_ret(alloc(c, Obj::Brush(Brush { style: 3, color: 0, hatch: 0, pattern: Some(ph) }))?)) // BS_PATTERN
}

/// `NtGdiCreatePen(style, width, color, brush)`.
fn nt_gdi_create_pen(c: &mut Call<'_>) -> R64 {
    let (style, width, color) = (c.a32(0), c.a32(1), c.a32(2));
    Ok(handle_ret(alloc(c, Obj::Pen(Pen { style, width, color }))?))
}

/// `NtGdiHfontCreate(logfont, size, ...)`: fonts are kept for GetObject;
/// drawing text is not implemented yet.
fn nt_gdi_hfont_create(c: &mut Call<'_>) -> R64 {
    let (lf, size) = (c.a(0), c.a32(1));
    let n = (size as usize).min(92).max(0);
    let mut data = c.g.bytes(lf, n)?;
    data.resize(92, 0);
    Ok(handle_ret(alloc(c, Obj::Font(data))?))
}

/// `NtGdiDeleteObjectApp(handle)`.
fn nt_gdi_delete_object_app(c: &mut Call<'_>) -> R64 {
    let h = c.a32(0);
    let idx = {
        let g = c.p.gdi.lock().unwrap();
        match index(&g, h) {
            Some(i) => i,
            None => return Ok(0),
        }
    };
    if idx < FIRST_HANDLE + 24 || c.p.gdi.lock().unwrap().sys_brushes.values().any(|&b| b & 0xffff == idx) {
        return Ok(1);
    }
    // A DC selected into nothing goes; a bitmap selected into a DC stays
    // until deselected, as Windows does.
    if let Some(Obj::Bitmap(b)) = get(c, h) {
        if b.selected {
            return Ok(1);
        }
    }
    match free(c, h)? {
        Some(Obj::Bitmap(b)) => b.release(c)?,
        Some(Obj::Dc(d)) => dc::release(c, d)?,
        Some(Obj::Brush(Brush { pattern: Some(p), .. })) => {
            if let Some(Obj::Bitmap(b)) = free(c, p)? {
                b.release(c)?;
            }
        }
        _ => {}
    }
    Ok(1)
}

/// `NtGdiExtGetObjectW(handle, count, buffer)`: bytes written (or needed).
fn nt_gdi_ext_get_object_w(c: &mut Call<'_>) -> R64 {
    let (h, count, buf) = (c.a32(0), c.a32(1) as i32, c.a(2));
    let Some(o) = get(c, h) else { return Ok(0) };
    let abi = c.p.abi;
    match o {
        Obj::Bitmap(b) => b.get_object(c, count, buf),
        Obj::Brush(br) => {
            let size = off!(abi, logbrush::SIZEOF);
            if buf == 0 {
                return Ok(size);
            }
            let n = (count as u64).min(size);
            let mut v = vec![0u8; size as usize];
            v[off!(abi, logbrush::LBSTYLE) as usize..][..4].copy_from_slice(&br.style.to_le_bytes());
            v[off!(abi, logbrush::LBCOLOR) as usize..][..4].copy_from_slice(&br.color.to_le_bytes());
            c.g.write(buf, &v[..n as usize])?;
            Ok(n)
        }
        Obj::Pen(p) => {
            let size = off!(abi, logpen::SIZEOF);
            if buf == 0 {
                return Ok(size);
            }
            let n = (count as u64).min(size);
            let mut v = vec![0u8; size as usize];
            v[0..4].copy_from_slice(&p.style.to_le_bytes());
            v[4..8].copy_from_slice(&p.width.to_le_bytes());
            v[12..16].copy_from_slice(&p.color.to_le_bytes());
            c.g.write(buf, &v[..n as usize])?;
            Ok(n)
        }
        Obj::Font(lf) => {
            if buf == 0 {
                return Ok(92);
            }
            let n = (count as usize).min(92);
            c.g.write(buf, &lf[..n])?;
            Ok(n as u64)
        }
        Obj::Palette(p) => {
            if buf == 0 || count < 2 {
                return Ok(2);
            }
            c.g.w16(buf, p.len() as u16)?;
            Ok(2)
        }
        _ => Err(c.unimplemented(&format!("GetObject of a {:#x} object", o.kind()))),
    }
}
