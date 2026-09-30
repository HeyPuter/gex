//! Bitmaps: device-dependent bitmaps (32 bpp like the display, or
//! monochrome) and DIB sections, with their pixels in guest memory
//! (win32u's bitmap.c and dib.c).

use super::super::user::{handle_ret, R64};
use super::{alloc, get, obj, Obj};
use crate::nt::{off, st, Call, Status, R};

/// The default palette's 20 system colors (0x00RRGGBB).
pub const DEFAULT_PALETTE: [u32; 20] = [
    0x000000, 0x800000, 0x008000, 0x808000, 0x000080, 0x800080, 0x008080, 0xc0c0c0, 0xc0dcc0, 0xa6caf0, 0xfffbf0,
    0xa0a0a4, 0x808080, 0xff0000, 0x00ff00, 0xffff00, 0x0000ff, 0xff00ff, 0x00ffff, 0xffffff,
];

#[derive(Clone, Debug)]
pub struct Dib {
    pub top_down: bool,
    pub compression: u32,
    /// BI_BITFIELDS masks (red, green, blue).
    pub masks: [u32; 3],
    /// The color table, as 0x00RRGGBB.
    pub colors: Vec<u32>,
    /// The info header as the program gave it (for GetObject's DIBSECTION).
    pub header: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Bitmap {
    pub width: i32,
    pub height: i32,
    pub bpp: u16,
    pub planes: u16,
    pub stride: u32,
    /// Guest address of the pixels.
    pub bits: u64,
    /// Bytes allocated for them (0 when the memory is not the bitmap's own).
    pub alloc: u64,
    pub dib: Option<Dib>,
    pub selected: bool,
}

pub fn ddb_stride(width: i32, bpp: u16) -> u32 {
    (((width as u32 * bpp as u32) + 15) >> 4) * 2
}

pub fn dib_stride(width: i32, bpp: u16) -> u32 {
    (((width as u32 * bpp as u32) + 31) >> 5) * 4
}

impl Bitmap {
    /// A device-dependent bitmap: 1 bpp or the display's 32 bpp.
    pub fn new_ddb(c: &mut Call<'_>, width: i32, height: i32, bpp: u16) -> R<Bitmap> {
        let stride = ddb_stride(width, bpp);
        let size = (stride as u64 * height as u64).max(1);
        let bits = super::super::sysinfo::alloc_private(c, size, "[gdi bitmap]")?;
        Ok(Bitmap { width, height, bpp, planes: 1, stride, bits, alloc: size, dib: None, selected: false })
    }

    /// Where row `y` (top = 0) starts.
    pub fn row(&self, y: i32) -> u64 {
        let top_down = self.dib.as_ref().is_none_or(|d| d.top_down);
        let r = if top_down { y } else { self.height - 1 - y };
        self.bits + r as u64 * self.stride as u64
    }

    /// Pixel (x, y) as 0x00RRGGBB.
    pub fn get(&self, c: &Call<'_>, x: i32, y: i32) -> u32 {
        let row = self.row(y);
        let g = &c.g;
        match self.bpp {
            32 => {
                let v = g.u32(row + 4 * x as u64).unwrap_or(0);
                match &self.dib {
                    Some(d) if d.compression == 3 => from_masks(v, &d.masks),
                    _ => v & 0xffffff,
                }
            }
            24 => {
                let b = g.bytes(row + 3 * x as u64, 3).unwrap_or_else(|_| vec![0; 3]);
                (b[2] as u32) << 16 | (b[1] as u32) << 8 | b[0] as u32
            }
            16 => {
                let v = g.u16(row + 2 * x as u64).unwrap_or(0) as u32;
                let masks = match &self.dib {
                    Some(d) if d.compression == 3 => d.masks,
                    _ => [0x7c00, 0x03e0, 0x001f],
                };
                from_masks(v, &masks)
            }
            8 | 4 | 1 => {
                let bit = x as u64 * self.bpp as u64;
                let byte = g.u8(row + bit / 8).unwrap_or(0);
                let idx = match self.bpp {
                    8 => byte as u32,
                    4 => ((byte >> (4 - (bit % 8) as u8)) & 0xf) as u32,
                    _ => ((byte >> (7 - (bit % 8) as u8)) & 1) as u32,
                };
                self.color(idx)
            }
            _ => 0,
        }
    }

    fn color(&self, idx: u32) -> u32 {
        match &self.dib {
            Some(d) => d.colors.get(idx as usize).copied().unwrap_or(0),
            None if self.bpp == 1 => {
                if idx != 0 {
                    0xffffff
                } else {
                    0
                }
            }
            None => DEFAULT_PALETTE.get(idx as usize).copied().unwrap_or(0),
        }
    }

    /// The color table index nearest to `rgb`.
    fn nearest(&self, rgb: u32) -> u32 {
        let table: Vec<u32> = match &self.dib {
            Some(d) => d.colors.clone(),
            None if self.bpp == 1 => vec![0, 0xffffff],
            None => DEFAULT_PALETTE.to_vec(),
        };
        let dist = |a: u32, b: u32| {
            let d = |s: u32| ((a >> s & 0xff) as i32 - (b >> s & 0xff) as i32).pow(2);
            d(0) + d(8) + d(16)
        };
        table.iter().enumerate().min_by_key(|(_, &c)| dist(c, rgb)).map(|(i, _)| i as u32).unwrap_or(0)
    }

    /// Set pixel (x, y) from 0x00RRGGBB.
    pub fn set(&self, c: &mut Call<'_>, x: i32, y: i32, rgb: u32) {
        let row = self.row(y);
        match self.bpp {
            32 => {
                let v = match &self.dib {
                    Some(d) if d.compression == 3 => to_masks(rgb, &d.masks),
                    _ => rgb & 0xffffff,
                };
                let _ = c.g.w32(row + 4 * x as u64, v);
            }
            24 => {
                let _ = c.g.write(row + 3 * x as u64, &[rgb as u8, (rgb >> 8) as u8, (rgb >> 16) as u8]);
            }
            16 => {
                let masks = match &self.dib {
                    Some(d) if d.compression == 3 => d.masks,
                    _ => [0x7c00, 0x03e0, 0x001f],
                };
                let _ = c.g.w16(row + 2 * x as u64, to_masks(rgb, &masks) as u16);
            }
            8 | 4 | 1 => {
                let idx = self.nearest(rgb);
                let bit = x as u64 * self.bpp as u64;
                let at = row + bit / 8;
                let byte = c.g.u8(at).unwrap_or(0);
                let v = match self.bpp {
                    8 => idx as u8,
                    4 => {
                        let sh = 4 - (bit % 8) as u8;
                        (byte & !(0xf << sh)) | ((idx as u8 & 0xf) << sh)
                    }
                    _ => {
                        let sh = 7 - (bit % 8) as u8;
                        (byte & !(1 << sh)) | ((idx as u8 & 1) << sh)
                    }
                };
                let _ = c.g.w8(at, v);
            }
            _ => {}
        }
    }

    /// A private copy (pattern brushes keep one).
    pub fn duplicate(&self, c: &mut Call<'_>) -> R<Bitmap> {
        let size = self.stride as u64 * self.height as u64;
        let bits = super::super::sysinfo::alloc_private(c, size.max(1), "[gdi bitmap]")?;
        let data = c.g.bytes(self.bits, size as usize)?;
        c.g.write(bits, &data)?;
        Ok(Bitmap { bits, alloc: size.max(1), selected: false, ..self.clone() })
    }

    /// Free the pixels this bitmap owns.
    pub fn release(&self, c: &mut Call<'_>) -> R<()> {
        if self.alloc != 0 {
            super::super::sysinfo::free_private(c, self.bits)?;
        }
        Ok(())
    }

    /// GetObject: BITMAP, or DIBSECTION for a DIB section when there is
    /// room.
    pub fn get_object(&self, c: &mut Call<'_>, count: i32, buf: u64) -> R64 {
        let abi = c.p.abi;
        let bm_size = off!(abi, bitmap::SIZEOF);
        let ds_size = off!(abi, dibsection::SIZEOF);
        if buf == 0 {
            return Ok(if self.dib.is_some() { ds_size } else { bm_size });
        }
        let count = count as u64;
        if count < bm_size {
            return Ok(0);
        }
        let mut v = vec![0u8; ds_size as usize];
        let put = |v: &mut Vec<u8>, at: u64, x: &[u8]| v[at as usize..at as usize + x.len()].copy_from_slice(x);
        put(&mut v, off!(abi, bitmap::BMWIDTH), &self.width.to_le_bytes());
        put(&mut v, off!(abi, bitmap::BMHEIGHT), &self.height.to_le_bytes());
        let wb = if self.dib.is_some() { self.stride } else { ddb_stride(self.width, self.bpp) };
        put(&mut v, off!(abi, bitmap::BMWIDTHBYTES), &wb.to_le_bytes());
        put(&mut v, off!(abi, bitmap::BMPLANES), &self.planes.to_le_bytes());
        put(&mut v, off!(abi, bitmap::BMBITSPIXEL), &self.bpp.to_le_bytes());
        if let Some(d) = &self.dib {
            let ps = abi.ptr_size() as usize;
            put(&mut v, off!(abi, bitmap::BMBITS), &self.bits.to_le_bytes()[..ps]);
            if count >= ds_size {
                let h = off!(abi, dibsection::DSBMIH);
                let mut hdr = d.header.clone();
                hdr.resize(40, 0);
                hdr[0..4].copy_from_slice(&40u32.to_le_bytes());
                // biHeight positive for bottom-up; biSizeImage filled in.
                let size_image = self.stride * self.height as u32;
                hdr[20..24].copy_from_slice(&size_image.to_le_bytes());
                put(&mut v, h, &hdr[..40]);
                let bf = off!(abi, dibsection::DSBITFIELDS);
                for (i, m) in d.masks.iter().enumerate() {
                    put(&mut v, bf + 4 * i as u64, &m.to_le_bytes());
                }
                c.g.write(buf, &v)?;
                return Ok(ds_size);
            }
        }
        c.g.write(buf, &v[..bm_size as usize])?;
        Ok(bm_size)
    }
}

fn from_masks(v: u32, m: &[u32; 3]) -> u32 {
    let chan = |mask: u32| -> u32 {
        if mask == 0 {
            return 0;
        }
        let shift = mask.trailing_zeros();
        let bits = (mask >> shift).count_ones();
        let x = (v & mask) >> shift;
        if bits >= 8 {
            x >> (bits - 8)
        } else {
            (x << (8 - bits)) | (x >> (2 * bits).saturating_sub(8).min(31))
        }
    };
    chan(m[0]) << 16 | chan(m[1]) << 8 | chan(m[2])
}

fn to_masks(rgb: u32, m: &[u32; 3]) -> u32 {
    let chan = |c8: u32, mask: u32| -> u32 {
        if mask == 0 {
            return 0;
        }
        let shift = mask.trailing_zeros();
        let bits = (mask >> shift).count_ones();
        let x = if bits >= 8 { c8 << (bits - 8) } else { c8 >> (8 - bits) };
        (x << shift) & mask
    };
    chan(rgb >> 16 & 0xff, m[0]) | chan(rgb >> 8 & 0xff, m[1]) | chan(rgb & 0xff, m[2])
}

/// A parsed `BITMAPINFO`.
pub struct Info {
    pub width: i32,
    pub height: i32,
    pub bpp: u16,
    pub compression: u32,
    pub masks: [u32; 3],
    pub colors: Vec<u32>,
    pub header: Vec<u8>,
    pub header_size: u32,
}

/// Read a `BITMAPINFO` (a BITMAPINFOHEADER or later, or a
/// BITMAPCOREHEADER) and its color table, for DIB_RGB_COLORS.
pub fn read_info(c: &mut Call<'_>, bmi: u64, usage: u32) -> R<Info> {
    let size = c.g.u32(bmi)?;
    let (width, height, bpp, compression, clr_used) = if size == 12 {
        (c.g.u16(bmi + 4)? as i32, c.g.u16(bmi + 6)? as i16 as i32, c.g.u16(bmi + 10)?, 0, 0)
    } else if size >= 40 {
        (
            c.g.u32(bmi + 4)? as i32,
            c.g.u32(bmi + 8)? as i32,
            c.g.u16(bmi + 14)?,
            c.g.u32(bmi + 16)?,
            c.g.u32(bmi + 32)?,
        )
    } else {
        return Err(Status(st::INVALID_PARAMETER));
    };
    if !matches!(bpp, 1 | 4 | 8 | 16 | 24 | 32) {
        return Err(c.unimplemented(&format!("a DIB of {bpp} bits per pixel")));
    }
    let mut masks = match bpp {
        16 => [0x7c00, 0x03e0, 0x001f],
        _ => [0xff0000, 0x00ff00, 0x0000ff],
    };
    let mut colors = Vec::new();
    match compression {
        0 => {}
        // BI_BITFIELDS: the masks follow a BITMAPINFOHEADER, or are in a
        // V4/V5 header.
        3 => {
            let at = if size == 40 { bmi + 40 } else { bmi + 40 };
            for (i, m) in masks.iter_mut().enumerate() {
                *m = c.g.u32(at + 4 * i as u64)?;
            }
        }
        _ => return Err(c.unimplemented(&format!("a DIB with compression {compression}"))),
    }
    if bpp <= 8 {
        if usage != 0 {
            return Err(c.unimplemented("DIB_PAL_COLORS color tables"));
        }
        let n = if clr_used != 0 { clr_used.min(1 << bpp) } else { 1 << bpp };
        let (at, step) = if size == 12 { (bmi + 12, 3) } else { (bmi + size as u64, 4) };
        for i in 0..n as u64 {
            let b = c.g.bytes(at + i * step, 3)?;
            colors.push((b[2] as u32) << 16 | (b[1] as u32) << 8 | b[0] as u32);
        }
    }
    let header = c.g.bytes(bmi, (size as usize).min(124))?;
    Ok(Info { width, height, bpp, compression, masks, colors, header, header_size: size })
}

/// A bitmap view of client DIB bits (StretchDIBits' source).
pub fn client_dib(info: &Info, bits: u64) -> Bitmap {
    Bitmap {
        width: info.width,
        height: info.height.abs(),
        bpp: info.bpp,
        planes: 1,
        stride: dib_stride(info.width, info.bpp),
        bits,
        alloc: 0,
        dib: Some(Dib {
            top_down: info.height < 0,
            compression: info.compression,
            masks: info.masks,
            colors: info.colors.clone(),
            header: info.header.clone(),
        }),
        selected: false,
    }
}

/// `NtGdiCreateBitmap(width, height, planes, bpp, bits)`.
pub fn nt_gdi_create_bitmap(c: &mut Call<'_>) -> R64 {
    let (w, h, planes, bpp, bits) = (c.a32(0) as i32, c.a32(1) as i32, c.a32(2), c.a32(3), c.a(4));
    if w < 0 || h < 0 || planes > 32 || bpp > 32 {
        return Ok(0);
    }
    let bpp = (planes * bpp) as u16;
    let bpp = match bpp {
        1 => 1,
        2..=4 => 4,
        5..=8 => 8,
        9..=16 => 16,
        17..=24 => 24,
        25..=32 => 32,
        _ => return Ok(0),
    };
    if w == 0 || h == 0 {
        return Ok(handle_ret(super::stock(c, super::stock::DEFAULT_BITMAP)));
    }
    let bm = Bitmap::new_ddb(c, w, h, bpp)?;
    if bits != 0 {
        let size = (bm.stride * h as u32) as usize;
        let data = c.g.bytes(bits, size)?;
        c.g.write(bm.bits, &data)?;
    }
    Ok(handle_ret(alloc(c, Obj::Bitmap(bm))?))
}

/// `NtGdiCreateCompatibleBitmap(hdc, width, height)`: the DC's depth (a
/// memory DC holding a monochrome bitmap makes monochrome bitmaps).
pub fn nt_gdi_create_compatible_bitmap(c: &mut Call<'_>) -> R64 {
    let (hdc, w, h) = (c.a32(0), c.a32(1) as i32, c.a32(2) as i32);
    let Some(Obj::Dc(d)) = get(c, hdc) else { return Ok(0) };
    if w <= 0 || h <= 0 {
        return Ok(handle_ret(super::stock(c, super::stock::DEFAULT_BITMAP)));
    }
    let bpp = match get(c, d.bitmap) {
        Some(Obj::Bitmap(b)) if d.memory => b.bpp.max(1),
        _ => 32,
    };
    let bpp = if bpp == 1 { 1 } else { 32 };
    let bm = Bitmap::new_ddb(c, w, h, bpp)?;
    Ok(handle_ret(alloc(c, Obj::Bitmap(bm))?))
}

/// `NtGdiCreateDIBSection(hdc, section, offset, bmi, usage, header_size,
/// flags, color_space, *bits)`.
pub fn nt_gdi_create_dib_section(c: &mut Call<'_>) -> R64 {
    let (_hdc, section, _offset, bmi, usage, bits_p) = (c.a32(0), c.a(1), c.a32(2), c.a(3), c.a32(4), c.a(8));
    if section != 0 {
        return Err(c.unimplemented("CreateDIBSection on a file mapping"));
    }
    let info = read_info(c, bmi, usage)?;
    if info.width <= 0 || info.height == 0 {
        return Ok(0);
    }
    let stride = dib_stride(info.width, info.bpp);
    let size = stride as u64 * info.height.unsigned_abs() as u64;
    let bits = super::super::sysinfo::alloc_private(c, size.max(1), "[dib section]")?;
    let mut bm = client_dib(&info, bits);
    bm.alloc = size.max(1);
    if bits_p != 0 {
        c.g.wptr(bits_p, bits)?;
    }
    Ok(handle_ret(alloc(c, Obj::Bitmap(bm))?))
}

/// `NtGdiGetBitmapBits(bitmap, count, bits)`.
pub fn nt_gdi_get_bitmap_bits(c: &mut Call<'_>) -> R64 {
    let (h, count, bits) = (c.a32(0), c.a32(1) as i32, c.a(2));
    let Some(Obj::Bitmap(bm)) = get(c, h) else { return Ok(0) };
    let stride = ddb_stride(bm.width, bm.bpp);
    let total = stride as i64 * bm.height as i64;
    if bits == 0 {
        return Ok(total as u64);
    }
    let n = (count as i64).min(total).max(0) as u32;
    let rows = n / stride.max(1);
    for y in 0..rows as i32 {
        let row = c.g.bytes(bm.row(y), stride.min(bm.stride) as usize)?;
        c.g.write(bits + y as u64 * stride as u64, &row)?;
    }
    Ok((rows * stride) as u64)
}

/// `NtGdiSetBitmapBits(bitmap, count, bits)`.
pub fn nt_gdi_set_bitmap_bits(c: &mut Call<'_>) -> R64 {
    let (h, count, bits) = (c.a32(0), c.a32(1) as i32, c.a(2));
    let Some(Obj::Bitmap(bm)) = get(c, h) else { return Ok(0) };
    let stride = ddb_stride(bm.width, bm.bpp);
    let rows = ((count.max(0) as u32) / stride.max(1)).min(bm.height as u32);
    for y in 0..rows as i32 {
        let row = c.g.bytes(bits + y as u64 * stride as u64, stride.min(bm.stride) as usize)?;
        c.g.write(bm.row(y), &row)?;
    }
    Ok((rows * stride) as u64)
}

/// `NtGdiGetDIBitsInternal(hdc, bitmap, startscan, lines, bits, info,
/// coloruse, max_bits, max_info)`.
pub fn nt_gdi_get_dibits_internal(c: &mut Call<'_>) -> R64 {
    let (_hdc, h, start, lines, bits, info_p, usage) = (c.a32(0), c.a32(1), c.a32(2), c.a32(3), c.a(4), c.a(5), c.a32(6));
    let Some(Obj::Bitmap(bm)) = get(c, h) else { return Ok(0) };
    let size = c.g.u32(info_p)?;
    if size < 40 {
        return Err(c.unimplemented("GetDIBits with a BITMAPCOREHEADER"));
    }
    let bpp = c.g.u16(info_p + 14)?;
    if bits == 0 || bpp == 0 {
        // Describe the bitmap.
        c.g.w32(info_p + 4, bm.width as u32)?;
        let height = match &bm.dib {
            Some(d) if d.top_down => -bm.height,
            _ => bm.height,
        };
        c.g.w32(info_p + 8, height as u32)?;
        c.g.w16(info_p + 12, 1)?;
        c.g.w16(info_p + 14, bm.bpp)?;
        let comp = match (&bm.dib, bm.bpp) {
            (Some(d), _) => d.compression,
            (None, 16) | (None, 32) => 0,
            _ => 0,
        };
        c.g.w32(info_p + 16, comp)?;
        c.g.w32(info_p + 20, dib_stride(bm.width, bm.bpp) * bm.height as u32)?;
        return Ok(bm.height as u64);
    }
    let info = read_info(c, info_p, usage)?;
    let dst = client_dib(&info, bits);
    let lines = lines.min((bm.height as u32).saturating_sub(start));
    // Scan lines count from the bottom.
    for i in 0..lines as i32 {
        let sy = bm.height - 1 - (start as i32 + i);
        let dy = dst.height - 1 - i;
        if dy < 0 {
            break;
        }
        for x in 0..bm.width.min(dst.width) {
            let v = bm.get(c, x, sy);
            dst.set(c, x, dy, v);
        }
    }
    Ok(lines as u64)
}

#[allow(dead_code)]
fn _unused() -> u32 {
    obj::BITMAP
}
