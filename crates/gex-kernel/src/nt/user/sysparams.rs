//! System metrics, colors, DPI and the one monitor (win32u's sysparams.c).
//!
//! gex presents one monitor at 96 dpi, 1920x1080 unless a program changes
//! the mode (ChangeDisplaySettings, as a fullscreen game does). The metrics
//! and colors are what Wine 11.15 reports for 1920x1080 with its default
//! theme, measured under native Wine (Xvfb, 1920x1080) and recorded here,
//! so programs see Wine's numbers; the screen-sized ones follow the mode.

use super::window::Rect;
use super::{err, handle_ret, set_last_error, write_rect, R64};
use crate::nt::{Call, R};

pub const SCREEN_W: i32 = 1920;
pub const SCREEN_H: i32 = 1080;

/// A display mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub width: i32,
    pub height: i32,
    pub bpp: u32,
}

impl Mode {
    /// The mode at start, and the registry's.
    pub const DEFAULT: Mode = Mode { width: SCREEN_W, height: SCREEN_H, bpp: 32 };

    pub const fn pack(self) -> u64 {
        (self.width as u64) | (self.height as u64) << 24 | (self.bpp as u64) << 48
    }

    fn unpack(v: u64) -> Mode {
        Mode { width: (v & 0xff_ffff) as i32, height: (v >> 24 & 0xff_ffff) as i32, bpp: (v >> 48) as u32 }
    }

    /// The mode at start: [`Mode::DEFAULT`], or `GEX_NT_DESKTOP=WxH` (one of
    /// the sizes on offer). A smaller desktop is a smaller DirectDraw
    /// primary surface, which wined3d moves between texture and memory
    /// whole (Peggle, windowed, reads back and re-uploads it twice a frame).
    pub fn startup() -> Mode {
        static M: std::sync::OnceLock<Mode> = std::sync::OnceLock::new();
        *M.get_or_init(|| {
            std::env::var("GEX_NT_DESKTOP")
                .ok()
                .and_then(|v| {
                    let (w, h) = v.split_once('x')?;
                    let (w, h) = (w.parse().ok()?, h.parse().ok()?);
                    SIZES.contains(&(w, h)).then_some(Mode { width: w, height: h, bpp: 32 })
                })
                .unwrap_or(Mode::DEFAULT)
        })
    }

    /// The mode in `nt::Process::display_mode`.
    pub fn unpack_current(v: &std::sync::atomic::AtomicU64) -> Mode {
        Mode::unpack(v.load(std::sync::atomic::Ordering::Relaxed))
    }
}

/// The modes on offer, in EnumDisplaySettings order: each size at 32, 16 and
/// 8 bpp, as Wine's virtual modes are.
const SIZES: [(i32, i32); 8] = [(640, 480), (800, 600), (1024, 768), (1280, 720), (1280, 960), (1280, 1024), (1600, 900), (1920, 1080)];
const DEPTHS: [u32; 3] = [32, 16, 8];

fn mode_at(index: usize) -> Option<Mode> {
    let (w, h) = *SIZES.get(index / DEPTHS.len())?;
    Some(Mode { width: w, height: h, bpp: DEPTHS[index % DEPTHS.len()] })
}

/// The current display mode.
pub fn mode(c: &Call<'_>) -> Mode {
    Mode::unpack_current(&c.p.display_mode)
}
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

pub fn system_metrics(c: &Call<'_>, index: i32) -> i32 {
    let m = mode(c);
    match index {
        // The screen, its full-screen client area, the maximized and
        // maximum-tracking sizes, the virtual screen: measured at 1920x1080,
        // offset by the mode's size.
        0 | 16 | 59 | 61 | 78 => METRICS[index as usize] - SCREEN_W + m.width,
        1 | 17 | 60 | 62 | 79 => METRICS[index as usize] - SCREEN_H + m.height,
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

pub fn primary_rect(c: &Call<'_>) -> Rect {
    let m = mode(c);
    Rect { left: 0, top: 0, right: m.width, bottom: m.height }
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
    // MONITORINFO, or MONITORINFOEXW (32 WCHARs of device name; user32
    // turns the A form into this one).
    if size != 40 && size != 104 {
        set_last_error(c, err::INVALID_PARAMETER);
        return Ok(0);
    }
    let r = primary_rect(c);
    write_rect(&mut c.g, info + 4, &r)?;
    write_rect(&mut c.g, info + 20, &r)?;
    c.g.w32(info + 36, 1)?; // MONITORINFOF_PRIMARY
    if size == 104 {
        let mut dev = crate::nt::to_w("\\\\.\\DISPLAY1");
        dev.resize(32, 0);
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
        // SPI_SETSCREENSAVEACTIVE, SPI_SETFILTERKEYS, SPI_SETTOGGLEKEYS,
        // SPI_SETSTICKYKEYS: what a fullscreen game turns off while it runs
        // and back on after. gex has no screensaver or accessibility keys to
        // change, so the settings stay as the GETs report them.
        0x11 | 0x33 | 0x35 | 0x3b => Ok(1),
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

/// `NtUserEnumDisplayMonitors(hdc, rect, proc, lparam)`: the one monitor,
/// through user32's `NtUserCallEnumDisplayMonitor` callback.
pub fn nt_user_enum_display_monitors(c: &mut Call<'_>) -> R64 {
    let (hdc, rect_p, proc_, lparam) = (c.a32(0), c.a(1), c.a(2), c.a(3));
    if hdc != 0 {
        return Err(c.unimplemented("EnumDisplayMonitors with a DC"));
    }
    let mut r = primary_rect(c);
    if rect_p != 0 {
        r = r.intersect(&super::read_rect(&c.g, rect_p)?);
        if r.is_empty() {
            return Ok(1);
        }
    }
    // struct enum_display_monitor_params { proc, monitor, hdc, rect, lparam }
    let ps = c.p.abi.ptr_size() as usize;
    let mut args = Vec::new();
    let mut put = |v: u64, n: usize| args.extend_from_slice(&v.to_le_bytes()[..n]);
    put(proc_, ps);
    put(MONITOR as u64, ps);
    put(0, ps);
    for v in [r.left, r.top, r.right, r.bottom] {
        put(v as u32 as u64, 4);
    }
    put(lparam, ps);
    let (_, ret) = super::winproc::user_callback(c, super::callbacks::CALL_ENUM_DISPLAY_MONITOR, &args)?;
    Ok((ret.len() == 4 && u32::from_le_bytes(ret[..4].try_into().unwrap()) != 0) as u64)
}

/// Is `device` (a `UNICODE_STRING *`, NULL for the primary) the display?
fn is_display(c: &Call<'_>, device: u64) -> R<bool> {
    let Some(name) = c.g.ustr(device)?.filter(|n| !n.is_empty()) else { return Ok(true) };
    Ok(crate::nt::wstr(&name).eq_ignore_ascii_case("\\\\.\\DISPLAY1"))
}

/// `NtUserEnumDisplaySettings(device, index, devmode, flags)`: the modes
/// on offer at 60 Hz, ENUM_CURRENT_SETTINGS (-1) the current one and
/// ENUM_REGISTRY_SETTINGS (-2) 1920x1080x32.
pub fn nt_user_enum_display_settings(c: &mut Call<'_>) -> R64 {
    let (device, index, dm) = (c.a(0), c.a32(1) as i32, c.a(2));
    if !is_display(c, device)? {
        return Ok(0);
    }
    let m = match index {
        -1 => mode(c),
        -2 => Mode::startup(),
        i if i >= 0 => match mode_at(i as usize) {
            Some(m) => m,
            None => return Ok(0),
        },
        _ => return Ok(0),
    };
    let mut name = crate::nt::to_w("Wine Display Driver");
    name.resize(32, 0);
    c.g.write_wchars(dm, &name)?;
    c.g.w16(dm + 64, 0x401)?; // dmSpecVersion
    c.g.w16(dm + 66, 0x401)?; // dmDriverVersion
    c.g.w16(dm + 68, 188)?; // dmSize: up to dmICMMethod
    c.g.w16(dm + 70, 0)?;
    // DM_POSITION | DM_DISPLAYORIENTATION | DM_BITSPERPEL | DM_PELSWIDTH |
    // DM_PELSHEIGHT | DM_DISPLAYFLAGS | DM_DISPLAYFREQUENCY
    c.g.w32(dm + 72, 0x20 | 0x80 | 0x40000 | 0x80000 | 0x100000 | 0x200000 | 0x400000)?;
    c.g.w32(dm + 76, 0)?;
    c.g.w32(dm + 80, 0)?;
    c.g.w32(dm + 84, 0)?; // DMDO_DEFAULT
    c.g.w32(dm + 168, m.bpp)?;
    c.g.w32(dm + 172, m.width as u32)?;
    c.g.w32(dm + 176, m.height as u32)?;
    c.g.w32(dm + 180, 0)?;
    c.g.w32(dm + 184, 60)?;
    Ok(1)
}

/// `NtUserEnumDisplayDevices(device, index, info, flags)`: the adapter
/// `\\.\DISPLAY1` and its one monitor. An NTSTATUS (user32 returns
/// `!status`), so a loop over the adapters ends at index 1.
pub fn nt_user_enum_display_devices(c: &mut Call<'_>) -> R64 {
    let (device, index, info) = (c.a(0), c.a32(1), c.a(2));
    // No name, or an empty one, enumerates the adapters.
    let adapter = c.g.ustr(device)?.is_none_or(|n| n.is_empty());
    if index != 0 || (!adapter && !is_display(c, device)?) {
        return Ok(crate::nt::st::UNSUCCESSFUL as u64);
    }
    let cb = c.g.u32(info)?;
    let put = |c: &mut Call<'_>, at: u64, n: usize, s: &str| -> R<()> {
        if at + 2 * n as u64 <= info + cb as u64 {
            let mut w = crate::nt::to_w(s);
            w.resize(n, 0);
            c.g.write_wchars(at, &w)?;
        }
        Ok(())
    };
    // DISPLAY_DEVICEW: cb, DeviceName[32], DeviceString[128], StateFlags,
    // DeviceID[128], DeviceKey[128].
    if adapter {
        put(c, info + 4, 32, "\\\\.\\DISPLAY1")?;
        put(c, info + 68, 128, "gex display adapter")?;
        c.g.w32(info + 324, 0x1 | 0x4)?; // ATTACHED_TO_DESKTOP | PRIMARY_DEVICE
        put(c, info + 328, 128, "PCI\\VEN_0000&DEV_0000&SUBSYS_00000000&REV_00")?;
        put(c, info + 584, 128, "\\Registry\\Machine\\System\\CurrentControlSet\\Control\\Video\\{00000000-0000-0000-0000-000000000000}\\0000")?;
    } else {
        put(c, info + 4, 32, "\\\\.\\DISPLAY1\\Monitor0")?;
        put(c, info + 68, 128, "Generic Non-PnP Monitor")?;
        c.g.w32(info + 324, 0x1 | 0x2)?; // ACTIVE | ATTACHED
        put(c, info + 328, 128, "MONITOR\\Default_Monitor\\{4d36e96e-e325-11ce-bfc1-08002be10318}\\0000")?;
        put(c, info + 584, 128, "\\Registry\\Machine\\System\\CurrentControlSet\\Control\\Class\\{4d36e96e-e325-11ce-bfc1-08002be10318}\\0000")?;
    }
    Ok(0)
}

/// `NtUserChangeDisplaySettings(device, devmode, hwnd, flags, lparam)`: a
/// `DISP_CHANGE_*`. A mode on offer becomes current (the desktop window
/// takes its size); no devmode, or no fields, restores the registry's.
/// `CDS_TEST` only checks.
pub fn nt_user_change_display_settings(c: &mut Call<'_>) -> R64 {
    const SUCCESSFUL: i32 = 0;
    const BADMODE: i32 = -2;
    const BADPARAM: i32 = -5;
    const CDS_TEST: u32 = 2;
    let (device, dm, flags) = (c.a(0), c.a(1), c.a32(3));
    if !is_display(c, device)? {
        return Ok(BADPARAM as u32 as u64);
    }
    let cur = mode(c);
    let fields = if dm != 0 { c.g.u32(dm + 72)? } else { 0 };
    let want = if fields == 0 {
        Mode::startup()
    } else {
        // DM_BITSPERPEL, DM_PELSWIDTH, DM_PELSHEIGHT; the rest of the mode
        // stays as it is.
        let f = |bit: u32, at: u64, v: u32| -> R<u32> { Ok(if fields & bit != 0 { c.g.u32(dm + at)? } else { v }) };
        Mode {
            bpp: f(0x40000, 168, cur.bpp)?,
            width: f(0x80000, 172, cur.width as u32)? as i32,
            height: f(0x100000, 176, cur.height as u32)? as i32,
        }
    };
    if !(0..SIZES.len() * DEPTHS.len()).any(|i| mode_at(i) == Some(want)) {
        return Ok(BADMODE as u32 as u64);
    }
    if flags & CDS_TEST == 0 && want != cur {
        c.p.display_mode.store(want.pack(), std::sync::atomic::Ordering::Relaxed);
        let mut u = c.p.user.lock().unwrap();
        let desktop = u.desktop;
        if let Some(d) = super::window::find_mut(&mut u, desktop) {
            let r = Rect { left: 0, top: 0, right: want.width, bottom: want.height };
            d.window_rect = r;
            d.client_rect = r;
        }
    }
    Ok(SUCCESSFUL as u64)
}
