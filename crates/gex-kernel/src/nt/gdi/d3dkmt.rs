//! D3DKMT, the kernel-mode display interface: just enough of it for
//! wined3d (ddraw, d3d8/9) to open its adapter and size its video memory.
//! There is one adapter, `\\.\DISPLAY1`'s (`user::sysparams`); its handles
//! name nothing, as no call here allocates or schedules anything.

use super::super::{st, Abi, Call, Status};
use super::R64;

/// The one adapter's handle, and the one device's.
const ADAPTER: u32 = 0x4000_0001;
const DEVICE: u32 = 0x4000_0002;
/// The adapter's LUID, as GDI reports it.
const LUID: u32 = 0x0000_6765;
/// What `QueryVideoMemoryInfo` reports as the local segment's budget.
const VIDEO_MEMORY: u64 = 1 << 30;

/// `NtGdiDdDDIOpenAdapterFromLuid(D3DKMT_OPENADAPTERFROMLUID *)`:
/// `{ LUID AdapterLuid; D3DKMT_HANDLE hAdapter; }`.
pub fn open_adapter_from_luid(c: &mut Call<'_>) -> R64 {
    let d = c.a(0);
    c.g.w32(d + 8, ADAPTER)?;
    Ok(st::SUCCESS as u64)
}

/// `D3DKMTOpenAdapterFromGdiDisplayName` (an `NtUserCallOneParam`):
/// `{ WCHAR DeviceName[32]; D3DKMT_HANDLE hAdapter; LUID AdapterLuid;
/// VidPnSourceId; }`.
pub fn open_adapter_from_gdi_display_name(c: &mut Call<'_>, d: u64) -> R64 {
    let name = c.g.wchars(d, 32)?;
    let end = name.iter().position(|&w| w == 0).unwrap_or(32);
    if !crate::nt::wstr(&name[..end]).eq_ignore_ascii_case("\\\\.\\DISPLAY1") {
        return Ok(st::UNSUCCESSFUL as u64);
    }
    c.g.w32(d + 64, ADAPTER)?;
    c.g.w32(d + 68, LUID)?;
    c.g.w32(d + 72, 0)?;
    c.g.w32(d + 76, 0)?;
    Ok(st::SUCCESS as u64)
}

/// `NtGdiDdDDICloseAdapter(D3DKMT_CLOSEADAPTER *)`, and every call that
/// takes a handle and owes nothing back: destroying a device, a queued-frame
/// limit, the VidPN ownership check.
pub fn check_adapter(c: &mut Call<'_>) -> R64 {
    let h = c.g.u32(c.a(0))?;
    if h != ADAPTER && h != DEVICE {
        return Err(Status(st::INVALID_PARAMETER));
    }
    Ok(st::SUCCESS as u64)
}

/// `NtGdiDdDDICreateDevice(D3DKMT_CREATEDEVICE *)`:
/// `{ union { D3DKMT_HANDLE hAdapter; void *pAdapter; }; Flags; hDevice; … }`.
pub fn create_device(c: &mut Call<'_>) -> R64 {
    let d = c.a(0);
    if c.g.u32(d)? != ADAPTER {
        return Err(Status(st::INVALID_PARAMETER));
    }
    let at = if c.p.abi == Abi::P64 { 12 } else { 8 };
    c.g.w32(d + at, DEVICE)?;
    Ok(st::SUCCESS as u64)
}

/// `NtGdiDdDDIQueryVideoMemoryInfo(D3DKMT_QUERYVIDEOMEMORYINFO *)`:
/// `{ HANDLE hProcess; D3DKMT_HANDLE hAdapter; MemorySegmentGroup; UINT64
/// Budget, CurrentUsage, CurrentReservation, AvailableForReservation,
/// CurrentCommitment; … }`, the UINT64s at 16 in both ABIs.
pub fn query_video_memory_info(c: &mut Call<'_>) -> R64 {
    let d = c.a(0);
    let ps = c.g.psize();
    if c.g.u32(d + ps)? != ADAPTER {
        return Err(Status(st::INVALID_PARAMETER));
    }
    // The non-local segment group (1) has nothing.
    let budget = if c.g.u32(d + ps + 4)? == 0 { VIDEO_MEMORY } else { 0 };
    c.g.w64(d + 16, budget)?;
    c.g.w64(d + 24, 0)?;
    c.g.w64(d + 32, 0)?;
    c.g.w64(d + 40, budget)?;
    c.g.w64(d + 48, 0)?;
    Ok(st::SUCCESS as u64)
}

/// `D3DDDIFORMAT`s a DC can be made over: (format, bpp, BI_BITFIELDS
/// masks, palette entries), as win32u's `NtGdiDdDDICreateDCFromMemory`.
const DC_FORMATS: &[(u32, u16, [u32; 3], u32)] = &[
    (20, 24, [0; 3], 0),                         // R8G8B8
    (21, 32, [0; 3], 0),                         // A8R8G8B8
    (22, 32, [0; 3], 0),                         // X8R8G8B8
    (23, 16, [0xf800, 0x07e0, 0x001f], 0),       // R5G6B5
    (24, 16, [0x7c00, 0x03e0, 0x001f], 0),       // X1R5G5B5
    (25, 16, [0x7c00, 0x03e0, 0x001f], 0),       // A1R5G5B5
    (26, 16, [0x0f00, 0x00f0, 0x000f], 0),       // A4R4G4B4
    (30, 16, [0x0f00, 0x00f0, 0x000f], 0),       // X4R4G4B4
    (41, 8, [0; 3], 256),                        // P8
];

/// `NtGdiDdDDICreateDCFromMemory(D3DKMT_CREATEDCFROMMEMORY *)`: a memory
/// DC with a DIB section over the caller's pixels selected (wined3d's GDI
/// renderer draws and presents through these). `{ void *pMemory; Format;
/// Width; Height; Pitch; HDC hDeviceDc; PALETTEENTRY *pColorTable; HDC hDc;
/// HANDLE hBitmap; }`.
pub fn create_dc_from_memory(c: &mut Call<'_>) -> R64 {
    use super::bitmap::{client_dib, dib_stride, Info};
    let d = c.a(0);
    let ps = c.g.psize();
    let o = |i: u64| if ps == 8 { [0, 8, 12, 16, 20, 24, 32, 40, 48][i as usize] } else { 4 * i };
    let (mem, format, width, height, pitch) =
        (c.g.ptr(d)?, c.g.u32(d + o(1))?, c.g.u32(d + o(2))?, c.g.u32(d + o(3))?, c.g.u32(d + o(4))?);
    let Some(&(_, bpp, masks, palette)) = DC_FORMATS.iter().find(|f| f.0 == format) else {
        return Err(Status(st::INVALID_PARAMETER));
    };
    if mem == 0 || width == 0 || height == 0 || width > 0x7fff || height > 0x7fff || pitch < dib_stride(width as i32, bpp) {
        return Err(Status(st::INVALID_PARAMETER));
    }
    let mut colors = Vec::new();
    if palette != 0 {
        let table = c.g.ptr(d + o(6))?;
        if table == 0 {
            return Err(Status(st::INVALID_PARAMETER));
        }
        // PALETTEENTRY { peRed, peGreen, peBlue, peFlags }.
        for i in 0..palette as u64 {
            let e = c.g.u32(table + 4 * i)?;
            colors.push((e & 0xff) << 16 | (e & 0xff00) | (e >> 16 & 0xff));
        }
    }
    let compression: u32 = if masks[0] != 0 { 3 } else { 0 };
    // The BITMAPINFOHEADER a DIBSECTION reports: top-down.
    let mut header = vec![0u8; 40];
    header[0..4].copy_from_slice(&40u32.to_le_bytes());
    header[4..8].copy_from_slice(&width.to_le_bytes());
    header[8..12].copy_from_slice(&(-(height as i32)).to_le_bytes());
    header[12..14].copy_from_slice(&1u16.to_le_bytes());
    header[14..16].copy_from_slice(&bpp.to_le_bytes());
    header[16..20].copy_from_slice(&compression.to_le_bytes());
    header[20..24].copy_from_slice(&(pitch * height).to_le_bytes());
    header[32..36].copy_from_slice(&(colors.len() as u32).to_le_bytes());
    let info = Info {
        width: width as i32,
        height: -(height as i32),
        bpp,
        compression,
        masks,
        colors,
        header,
        header_size: 40,
    };
    let mut bm = client_dib(&info, mem);
    bm.stride = pitch;
    let hbm = super::alloc(c, super::Obj::Bitmap(bm))?;
    let hdc = super::dc::create(c, true, 0, false)?;
    super::dc::select_bitmap(c, hdc, hbm)?;
    c.g.wptr(d + o(7), super::super::user::handle_ret(hdc))?;
    c.g.wptr(d + o(8), super::super::user::handle_ret(hbm))?;
    Ok(st::SUCCESS as u64)
}

/// `NtGdiDdDDIDestroyDCFromMemory(D3DKMT_DESTROYDCFROMMEMORY *)`:
/// `{ HDC hDc; HANDLE hBitmap; }`. The pixels stay the caller's.
pub fn destroy_dc_from_memory(c: &mut Call<'_>) -> R64 {
    let d = c.a(0);
    let ps = c.g.psize();
    let (hdc, hbm) = (c.g.ptr(d)? as u32, c.g.ptr(d + ps)? as u32);
    if !matches!(super::get(c, hdc), Some(super::Obj::Dc(_))) || !matches!(super::get(c, hbm), Some(super::Obj::Bitmap(_))) {
        return Err(Status(st::INVALID_PARAMETER));
    }
    if let Some(super::Obj::Dc(dc)) = super::free(c, hdc)? {
        super::dc::release(c, dc)?;
    }
    super::free(c, hbm)?;
    Ok(st::SUCCESS as u64)
}
