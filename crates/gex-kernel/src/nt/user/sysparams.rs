//! System metrics, colors, DPI and the one monitor (win32u's sysparams.c).
//!
//! gex presents a fixed display: one 1920x1080 monitor at 96 dpi. The
//! metrics and colors are what Wine 11.15 reports for that display with its
//! default theme, measured under native Wine (Xvfb, 1920x1080) and recorded
//! here, so programs see Wine's numbers.

use super::window::Rect;
use super::{err, handle_ret, set_last_error, write_rect, R64};
use crate::nt::{Call, R};

pub const SCREEN_W: i32 = 1920;
pub const SCREEN_H: i32 = 1080;
pub const DPI: u32 = 96;
/// The one monitor's handle.
pub const MONITOR: u32 = 0x0001_0001;

/// `GetSystemMetrics(0..100)` under Wine 11.15, 1920x1080, default theme.
const METRICS: [i32; 100] = [
    1920, 1080, 17, 17, 26, 1, 1, 3, 3, 17, 17, 32, 32, 32, 32, 19, 1920, 1098, 0, 1, 17, 17, 0, 0, 0, 0, 0, 0,
    119, 34, 18, 25, 4, 4, 119, 34, 4, 4, 75, 75, 0, 0, 0, 3, 0, 2, 2, 160, 31, 16, 16, 18, 17, 17, 18, 18, 8,
    160, 31, 1932, 1092, 1928, 1132, 3, 0, 0, 0, 0, 4, 4, 0, 13, 13, 0, 0, 1, 0, 0, 1920, 1080, 1, 1, 0, 1, 1,
    0, 0, 0, 0, 0, 90, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

/// `GetSysColor(0..31)` under Wine 11.15's default theme.
const COLORS: [u32; 31] = [
    0xffffff, 0x956f25, 0xfa9632, 0x808080, 0xffffff, 0xffffff, 0x9e9e9e, 0x000000, 0x000000, 0x000000, 0xffffff,
    0xffffff, 0x808080, 0xfa9630, 0xffffff, 0xf5f5f5, 0xa6a6a6, 0x6a6a6a, 0x000000, 0xc8c8c8, 0xffffff, 0x6a6a6a,
    0xe3e3e3, 0x000000, 0xffffff, 0xffffff, 0xfa9630, 0xfa9632, 0x808080, 0xfa9630, 0xffffff,
];

/// Settable system parameters.
#[derive(Debug)]
pub struct SysParams {
    pub layout: u32,
}

impl Default for SysParams {
    fn default() -> Self {
        SysParams { layout: 0 }
    }
}

pub fn system_metrics(_c: &Call<'_>, index: i32) -> i32 {
    match index {
        0..=99 => METRICS[index as usize],
        // SM_REMOTESESSION, SM_SHUTTINGDOWN, SM_REMOTECONTROL,
        // SM_CARETBLINKINGENABLED, SM_CONVERTIBLESLATEMODE, SM_SYSTEMDOCKED
        0x1000..=0x1003 | 0x2003 | 0x2004 => 0,
        _ => 0,
    }
}

pub fn sys_color(index: i32) -> u32 {
    COLORS.get(index as usize).copied().unwrap_or(0)
}

pub fn primary_rect(_c: &Call<'_>) -> Rect {
    Rect { left: 0, top: 0, right: SCREEN_W, bottom: SCREEN_H }
}

/// `NtUserGetSystemDpiForProcess(process)`.
pub fn nt_user_get_system_dpi_for_process(_c: &mut Call<'_>) -> R64 {
    Ok(DPI as u64)
}

/// `NtUserGetProcessDpiAwarenessContext(process)`.
pub fn nt_user_get_process_dpi_awareness_context(c: &mut Call<'_>) -> R64 {
    Ok(c.p.user.lock().unwrap().process_dpi_context as u64)
}

/// `NtUserSetProcessDpiAwarenessContext(context, unknown)`.
pub fn nt_user_set_process_dpi_awareness_context(c: &mut Call<'_>) -> R64 {
    let ctx = c.a32(0);
    // Every DPI is 96 here, so awareness changes nothing visible; the value
    // is kept for the queries.
    let ctx = if ctx & 0xff00 == 0 { ctx | DPI << 8 } else { ctx };
    c.p.user.lock().unwrap().process_dpi_context = ctx | 0x8000_0000;
    Ok(1)
}

/// `NtUserSetThreadDpiAwarenessContext`: returns the previous one. The value
/// lives in `ntuser_thread_info.dpi_context`, which user32 reads directly.
pub fn set_thread_dpi_awareness_context(c: &mut Call<'_>, ctx: u32) -> R<u32> {
    let at = super::client_info(c) + crate::nt::off!(c.p.abi, struct_ntuser_thread_info::DPI_CONTEXT);
    let prev = c.g.u32(at)?;
    let prev = if prev == 0 { c.p.user.lock().unwrap().process_dpi_context } else { prev };
    c.g.w32(at, ctx)?;
    Ok(prev)
}

/// `NtUserGetProcessDefaultLayout(*layout)`.
pub fn nt_user_get_process_default_layout(c: &mut Call<'_>) -> R64 {
    let p = c.a(0);
    let l = c.p.user.lock().unwrap().sys.layout;
    c.g.w32(p, l)?;
    Ok(1)
}

/// `NtUserSetProcessDefaultLayout(layout)`.
pub fn nt_user_set_process_default_layout(c: &mut Call<'_>) -> R64 {
    let l = c.a32(0);
    c.p.user.lock().unwrap().sys.layout = l;
    Ok(1)
}

/// `MONITORINFO(EX)` for the one monitor.
pub fn get_monitor_info(c: &mut Call<'_>, monitor: u32, info: u64) -> R64 {
    if monitor != MONITOR {
        set_last_error(c, err::INVALID_PARAMETER);
        return Ok(0);
    }
    let size = c.g.u32(info)?;
    if size != 40 && size != 72 {
        set_last_error(c, err::INVALID_PARAMETER);
        return Ok(0);
    }
    let r = primary_rect(c);
    write_rect(&mut c.g, info + 4, &r)?;
    write_rect(&mut c.g, info + 20, &r)?;
    c.g.w32(info + 36, 1)?; // MONITORINFOF_PRIMARY
    if size == 72 {
        let mut dev = crate::nt::to_w("\\\\.\\DISPLAY1");
        dev.resize(16, 0);
        c.g.write_wchars(info + 40, &dev)?;
    }
    Ok(1)
}

/// `NONCLIENTMETRICSW` with Wine's values (measured: borders 1, scroll bars
/// 17, captions 18x25, small captions 17x17, menus 18x18, Tahoma -13).
fn write_nonclient_metrics(c: &mut Call<'_>, p: u64, size: u32) -> R<bool> {
    if size != 500 && size != 504 {
        return Ok(false);
    }
    c.g.zero(p + 4, size as usize - 4)?;
    for (at, v) in [(4, 1), (8, 17), (12, 17), (16, 18), (20, 25), (116, 17), (120, 17), (216, 18), (220, 18)] {
        c.g.w32(p + at, v)?;
    }
    // LOGFONTW: lfHeight, ..., lfWeight at 16, lfCharSet at 23, lfFaceName at 28.
    for (at, weight) in [(24u64, 400u32), (124, 400), (224, 400), (316, 400), (408, 400)] {
        c.g.w32(p + at, (-13i32) as u32)?;
        c.g.w32(p + at + 16, weight)?;
        c.g.w8(p + at + 23, 1)?;
        c.g.write_wchars(p + at + 28, &crate::nt::to_w("Tahoma"))?;
    }
    Ok(true)
}

fn spi(c: &mut Call<'_>, action: u32, val: u32, ptr: u64, _winini: u32) -> R64 {
    const SPI_GETBEEP: u32 = 0x0001;
    const SPI_GETMOUSE: u32 = 0x0003;
    const SPI_GETBORDER: u32 = 0x0005;
    const SPI_GETKEYBOARDSPEED: u32 = 0x000a;
    const SPI_GETSCREENSAVETIMEOUT: u32 = 0x000e;
    const SPI_GETSCREENSAVEACTIVE: u32 = 0x0010;
    const SPI_GETKEYBOARDDELAY: u32 = 0x0016;
    const SPI_GETWORKAREA: u32 = 0x0030;
    const SPI_GETNONCLIENTMETRICS: u32 = 0x0029;
    const SPI_GETSHOWSOUNDS: u32 = 0x0038;
    const SPI_GETSTICKYKEYS: u32 = 0x003a;
    const SPI_GETTOGGLEKEYS: u32 = 0x0034;
    const SPI_GETFILTERKEYS: u32 = 0x0032;
    const SPI_GETMOUSESPEED: u32 = 0x0070;
    const SPI_GETWHEELSCROLLLINES: u32 = 0x0068;
    const SPI_GETMENUSHOWDELAY: u32 = 0x006a;
    const SPI_GETFONTSMOOTHING: u32 = 0x004a;
    const SPI_GETSCREENREADER: u32 = 0x0046;
    const SPI_GETHIGHCONTRAST: u32 = 0x0042;
    const SPI_GETWHEELSCROLLCHARS: u32 = 0x006c;
    const SPI_GETMOUSEHOVERTIME: u32 = 0x0066;
    const SPI_GETMOUSEHOVERWIDTH: u32 = 0x0062;
    const SPI_GETMOUSEHOVERHEIGHT: u32 = 0x0064;
    const SPI_GETFOCUSBORDERWIDTH: u32 = 0x200e;
    const SPI_GETFOCUSBORDERHEIGHT: u32 = 0x2010;
    let _ = val;
    let w32 = |c: &mut Call<'_>, v: u32| -> R64 {
        c.g.w32(ptr, v)?;
        Ok(1)
    };
    match action {
        SPI_GETBEEP | SPI_GETSCREENSAVEACTIVE => w32(c, 1),
        SPI_GETMOUSE => {
            // threshold 6, 10, acceleration 1
            c.g.w32(ptr, 6)?;
            c.g.w32(ptr + 4, 10)?;
            c.g.w32(ptr + 8, 1)?;
            Ok(1)
        }
        SPI_GETBORDER => w32(c, 1),
        SPI_GETKEYBOARDSPEED => w32(c, 31),
        SPI_GETKEYBOARDDELAY => w32(c, 1),
        SPI_GETSCREENSAVETIMEOUT => w32(c, 300),
        SPI_GETSHOWSOUNDS | SPI_GETSCREENREADER => w32(c, 0),
        SPI_GETMOUSESPEED => w32(c, 10),
        SPI_GETWHEELSCROLLLINES | SPI_GETWHEELSCROLLCHARS => w32(c, 3),
        SPI_GETMENUSHOWDELAY | SPI_GETMOUSEHOVERTIME => w32(c, 400),
        SPI_GETMOUSEHOVERWIDTH | SPI_GETMOUSEHOVERHEIGHT => w32(c, 4),
        SPI_GETFONTSMOOTHING => w32(c, 1),
        SPI_GETFOCUSBORDERWIDTH | SPI_GETFOCUSBORDERHEIGHT => w32(c, 1),
        SPI_GETWORKAREA => {
            let r = primary_rect(c);
            write_rect(&mut c.g, ptr, &r)?;
            Ok(1)
        }
        SPI_GETNONCLIENTMETRICS => {
            let size = c.g.u32(ptr)?;
            if !write_nonclient_metrics(c, ptr, size)? {
                set_last_error(c, err::INVALID_PARAMETER);
                return Ok(0);
            }
            Ok(1)
        }
        // STICKYKEYS / TOGGLEKEYS / FILTERKEYS / HIGHCONTRAST: all off,
        // cbSize kept.
        SPI_GETSTICKYKEYS | SPI_GETTOGGLEKEYS => {
            c.g.w32(ptr + 4, 0)?;
            Ok(1)
        }
        SPI_GETFILTERKEYS => {
            let size = c.g.u32(ptr)?;
            c.g.zero(ptr + 4, size.saturating_sub(4) as usize)?;
            Ok(1)
        }
        SPI_GETHIGHCONTRAST => {
            c.g.w32(ptr + 4, 0)?;
            let ps = c.g.psize();
            c.g.wptr(ptr + 8, 0)?;
            let _ = ps;
            Ok(1)
        }
        _ => Err(c.unimplemented(&format!("NtUserSystemParametersInfo action {action:#x}"))),
    }
}

/// `NtUserSystemParametersInfo(action, val, ptr, winini)`.
pub fn nt_user_system_parameters_info(c: &mut Call<'_>) -> R64 {
    let (action, val, ptr, winini) = (c.a32(0), c.a32(1), c.a(2), c.a32(3));
    spi(c, action, val, ptr, winini)
}

/// `NtUserSystemParametersInfoForDpi(action, val, ptr, winini, dpi)`.
pub fn nt_user_system_parameters_info_for_dpi(c: &mut Call<'_>) -> R64 {
    let (action, val, ptr, winini) = (c.a32(0), c.a32(1), c.a(2), c.a32(3));
    spi(c, action, val, ptr, winini)
}

#[allow(dead_code)]
fn _unused() -> u64 {
    handle_ret(0)
}
