//! USER: what win32u's unix side and wineserver's USER parts do for Wine's PE
//! `user32.dll` (docs/nt-layer.md §5, §6a, §10): window classes, the window
//! tree, message queues, the calls into user32 (`KeUserModeCallback`), input
//! state and system parameters.
//!
//! One [`User`] per process behind one lock. The lock is never held across a
//! call into the guest (a window procedure, any user32 callback): handlers
//! read what they need, drop it, call, and take it again.
//!
//! The layouts of Wine's private structures (`struct win_proc_params` and
//! friends, which are "not compatible with Windows") come from Wine 11.15's
//! `ntuser.h` through the generated `layout.rs`, like every other layout.

pub mod class;
pub mod defwnd;
pub mod message;
pub mod paint;
pub mod sysparams;
pub mod window;
pub mod winproc;

use super::{off, st, sysno, Abi, Call, Status, G, R};
use std::collections::{HashMap, VecDeque};

/// A win32u handler's result: the value for `rax` (a HWND, a BOOL, an
/// LRESULT, …), or a failure status. win32u reports failures as a zero result
/// and a last-error code, which [`dispatch`] does.
pub type R64 = Result<u64, Status>;

/// `NTUSER_NB_PROCS`: the client procedures user32 registers.
pub const NB_PROCS: usize = 17;
/// `NTUSER_NB_WORKERS`.
pub const NB_WORKERS: usize = 11;

/// `enum ntuser_client_procs` values used here.
pub mod procs {
    pub const SCROLLBAR: usize = 0;
    pub const MESSAGE: usize = 1;
    pub const MENU: usize = 2;
    pub const DESKTOP: usize = 3;
    pub const DEFWND: usize = 4;
    pub const ICONTITLE: usize = 5;
    pub const BUTTON: usize = 7;
    pub const COMBO: usize = 8;
    pub const COMBOLBOX: usize = 9;
    pub const DIALOG: usize = 10;
    pub const EDIT: usize = 11;
    pub const LISTBOX: usize = 12;
    pub const MDICLIENT: usize = 13;
    pub const STATIC: usize = 14;
    pub const IME: usize = 15;
}

/// `KernelCallbackTable` indices (`ALL_USER32_CALLBACKS`, ntuser.h).
pub mod callbacks {
    pub const CALL_DISPATCH_CALLBACK: u32 = 0;
    pub const CALL_ENUM_DISPLAY_MONITOR: u32 = 1;
    pub const CALL_SEND_ASYNC_CALLBACK: u32 = 2;
    pub const CALL_WIN_EVENT_HOOK: u32 = 3;
    pub const CALL_WIN_PROC: u32 = 4;
    pub const CALL_WINDOWS_HOOK: u32 = 5;
    pub const COPY_IMAGE: u32 = 6;
    pub const DRAW_NON_CLIENT_BUTTON: u32 = 7;
    pub const DRAW_SCROLL_BAR: u32 = 8;
    pub const DRAW_TEXT: u32 = 9;
    pub const FREE_CACHED_CLIPBOARD_DATA: u32 = 10;
    pub const IMM_PROCESS_KEY: u32 = 11;
    pub const IMM_TRANSLATE_MESSAGE: u32 = 12;
    pub const INIT_BUILTIN_CLASSES: u32 = 13;
    pub const LOAD_DRIVER: u32 = 14;
    pub const LOAD_IMAGE: u32 = 15;
    pub const LOAD_SYS_MENU: u32 = 16;
}

/// User handles: `FIRST_USER_HANDLE + 2 * index`, generation in the high word.
const FIRST_USER_HANDLE: u32 = 0x20;
const MAX_USER_HANDLES: usize = (0xffef - 0x20 + 1) >> 1;

/// `NTUSER_OBJ_*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjKind {
    Window,
    Menu,
    Icon,
    WinPos,
    Accel,
    Hook,
    Imc,
}

#[derive(Clone, Copy, Debug)]
struct HandleEntry {
    kind: ObjKind,
    generation: u16,
}

/// Per-thread USER state: the message queue and what hangs off it.
#[derive(Default, Debug)]
pub struct ThreadState {
    pub queue: message::Queue,
    /// `ntuser_thread_info.dpi_context` shadow.
    pub dpi_context: u32,
    pub focus: u32,
    pub active: u32,
    pub capture: u32,
    /// Recursion depth of window procedure calls on this thread.
    pub call_depth: u32,
}

/// The process's USER state.
pub struct User {
    /// Set by `NtUserInitializeClientPfnArrays`.
    pub initialized: bool,
    pub user32: u64,
    /// Registered winprocs: `(procA, procW)`, index = the low word of a
    /// `MAKE_WNDPROC` handle. The first [`NB_PROCS`] are user32's builtins.
    pub winprocs: Vec<(u64, u64)>,
    pub workers: [u64; NB_WORKERS],
    handles: Vec<Option<HandleEntry>>,
    free: Vec<usize>,
    pub classes: Vec<class::Class>,
    pub next_class_id: u32,
    /// Builtin class atoms, by client proc index.
    pub builtin_atoms: [u16; NB_PROCS],
    pub builtins_registered: bool,
    pub windows: HashMap<u32, window::Window>,
    pub desktop: u32,
    pub message_parent: u32,
    /// Registered window messages and class names: atoms from 0xc000.
    pub atoms: Vec<Vec<u16>>,
    pub threads: HashMap<u32, ThreadState>,
    pub foreground: u32,
    pub process_dpi_context: u32,
    pub sys: sysparams::SysParams,
}

impl User {
    pub fn new() -> User {
        User {
            initialized: false,
            user32: 0,
            winprocs: vec![(0, 0); NB_PROCS],
            workers: [0; NB_WORKERS],
            handles: Vec::new(),
            free: Vec::new(),
            classes: Vec::new(),
            next_class_id: 0,
            builtin_atoms: [0; NB_PROCS],
            builtins_registered: false,
            windows: HashMap::new(),
            desktop: 0,
            message_parent: 0,
            atoms: Vec::new(),
            threads: HashMap::new(),
            foreground: 0,
            // DPI_AWARENESS_UNAWARE, version 1, 96 dpi (NTUSER_DPI_UNAWARE).
            process_dpi_context: 0x6010,
            sys: sysparams::SysParams::default(),
        }
    }

    pub fn alloc_handle(&mut self, kind: ObjKind) -> Option<u32> {
        let index = match self.free.pop() {
            Some(i) => i,
            None => {
                if self.handles.len() >= MAX_USER_HANDLES {
                    return None;
                }
                self.handles.push(None);
                self.handles.len() - 1
            }
        };
        let generation = self.handles[index].map_or(1, |e| e.generation.wrapping_add(1).max(1));
        self.handles[index] = Some(HandleEntry { kind, generation });
        Some(FIRST_USER_HANDLE + 2 * index as u32 | (generation as u32) << 16)
    }

    pub fn free_handle(&mut self, h: u32) {
        if let Some(i) = self.handle_index(h) {
            // Keep the generation so the next handle in this slot differs.
            let generation = self.handles[i].map(|e| e.generation).unwrap_or(0);
            self.handles[i] = None;
            self.free.push(i);
            let _ = generation;
        }
    }

    fn handle_index(&self, h: u32) -> Option<usize> {
        let low = h & 0xffff;
        if low < FIRST_USER_HANDLE || low & 1 != 0 {
            return None;
        }
        let i = ((low - FIRST_USER_HANDLE) >> 1) as usize;
        let e = self.handles.get(i).copied().flatten()?;
        // A 16-bit handle (high word 0 or 0xffff) matches any generation.
        let hi = h >> 16;
        if hi != 0 && hi != 0xffff && hi != e.generation as u32 {
            return None;
        }
        Some(i)
    }

    /// The full handle for `h`, if it names a live object of `kind`.
    pub fn full_handle(&self, h: u32, kind: ObjKind) -> Option<u32> {
        let i = self.handle_index(h)?;
        let e = self.handles[i]?;
        (e.kind == kind).then_some(FIRST_USER_HANDLE + 2 * i as u32 | (e.generation as u32) << 16)
    }

    pub fn thread(&mut self, tid: u32) -> &mut ThreadState {
        self.threads.entry(tid).or_default()
    }

    /// Add or find a user atom (registered messages, class names).
    pub fn add_atom(&mut self, name: &[u16]) -> u16 {
        let key = lower(name);
        if let Some(i) = self.atoms.iter().position(|a| lower(a) == key) {
            return 0xc000 + i as u16;
        }
        self.atoms.push(name.to_vec());
        0xc000 + (self.atoms.len() - 1) as u16
    }

    pub fn find_atom(&self, name: &[u16]) -> Option<u16> {
        let key = lower(name);
        self.atoms.iter().position(|a| lower(a) == key).map(|i| 0xc000 + i as u16)
    }

    pub fn atom_name(&self, atom: u16) -> Option<Vec<u16>> {
        if atom < 0xc000 {
            return (atom != 0).then(|| super::to_w(&format!("#{atom}")));
        }
        self.atoms.get((atom - 0xc000) as usize).cloned()
    }
}

impl Default for User {
    fn default() -> Self {
        Self::new()
    }
}

pub fn lower(s: &[u16]) -> Vec<u16> {
    s.iter()
        .map(|&c| if (b'A' as u16..=b'Z' as u16).contains(&c) { c + 32 } else { c })
        .collect()
}

/// A name or an integer atom, as user32 passes class names (`UNICODE_STRING`
/// whose buffer may be `MAKEINTATOM`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NameOrAtom {
    Name(Vec<u16>),
    Atom(u16),
}

impl NameOrAtom {
    /// Integer atoms in string form (`#123`) become atoms.
    pub fn normalize(self) -> NameOrAtom {
        if let NameOrAtom::Name(n) = &self {
            if n.first() == Some(&(b'#' as u16)) && n.len() > 1 {
                if let Ok(v) = super::wstr(&n[1..]).parse::<u32>() {
                    if v > 0 && v < 0xc000 {
                        return NameOrAtom::Atom(v as u16);
                    }
                }
            }
        }
        self
    }
}

/// Read a `UNICODE_STRING *` that may carry an integer atom in its buffer.
pub fn read_name(g: &G<'_>, us: u64) -> R<Option<NameOrAtom>> {
    if us == 0 {
        return Ok(None);
    }
    let len = g.u16(us)? as usize;
    let buf = g.ptr(us + off!(g.abi, unicode_string::BUFFER))?;
    if buf >> 16 == 0 {
        return Ok(Some(NameOrAtom::Atom(buf as u16)));
    }
    Ok(Some(NameOrAtom::Name(g.wchars(buf, len / 2)?).normalize()))
}

/// A NUL-terminated UTF-16 string, or an integer atom (`IS_INTRESOURCE`).
pub fn read_wstr_or_atom(g: &G<'_>, p: u64) -> R<NameOrAtom> {
    if p >> 16 == 0 {
        return Ok(NameOrAtom::Atom(p as u16));
    }
    Ok(NameOrAtom::Name(read_wstrz(g, p)?).normalize())
}

/// A NUL-terminated UTF-16 string (at most 32k units).
pub fn read_wstrz(g: &G<'_>, p: u64) -> R<Vec<u16>> {
    let mut out = Vec::new();
    for i in 0..32768u64 {
        let c = g.u16(p + 2 * i)?;
        if c == 0 {
            break;
        }
        out.push(c);
    }
    Ok(out)
}

/// A NUL-terminated byte string (at most 32k bytes).
pub fn read_strz(g: &G<'_>, p: u64) -> R<Vec<u8>> {
    let mut out = Vec::new();
    for i in 0..32768u64 {
        let c = g.u8(p + i)?;
        if c == 0 {
            break;
        }
        out.push(c);
    }
    Ok(out)
}

/// Windows error codes win32u sets.
pub mod err {
    pub const INVALID_PARAMETER: u32 = 87;
    pub const INSUFFICIENT_BUFFER: u32 = 122;
    pub const INVALID_WINDOW_HANDLE: u32 = 1400;
    pub const INVALID_MENU_HANDLE: u32 = 1401;
    pub const INVALID_CURSOR_HANDLE: u32 = 1402;
    pub const CLASS_ALREADY_EXISTS: u32 = 1410;
    pub const CLASS_DOES_NOT_EXIST: u32 = 1411;
    pub const CLASS_HAS_WINDOWS: u32 = 1412;
    pub const INVALID_INDEX: u32 = 1413;
    pub const TLW_WITH_WSCHILD: u32 = 1406;
    pub const ACCESS_DENIED: u32 = 5;
    pub const NOT_ENOUGH_MEMORY: u32 = 8;
    pub const MESSAGE_SYNC_ONLY: u32 = 1159;
    pub const INVALID_THREAD_ID: u32 = 1444;
    pub const NOACCESS: u32 = 998;
    pub const TIMEOUT: u32 = 1460;
}

/// `SetLastError` on the calling thread.
pub fn set_last_error(c: &mut Call<'_>, code: u32) {
    let at = c.t.teb + off!(c.p.abi, teb::LASTERRORVALUE);
    let _ = c.g.w32(at, code);
}

/// The calling thread's `ntuser_thread_info` (`TEB->Win32ClientInfo`).
pub fn client_info(c: &Call<'_>) -> u64 {
    c.t.teb + off!(c.p.abi, teb::WIN32CLIENTINFO)
}

/// A `RECT` at `p`.
pub fn read_rect(g: &G<'_>, p: u64) -> R<window::Rect> {
    Ok(window::Rect {
        left: g.u32(p)? as i32,
        top: g.u32(p + 4)? as i32,
        right: g.u32(p + 8)? as i32,
        bottom: g.u32(p + 12)? as i32,
    })
}

pub fn write_rect(g: &mut G<'_>, p: u64, r: &window::Rect) -> R<()> {
    g.w32(p, r.left as u32)?;
    g.w32(p + 4, r.top as u32)?;
    g.w32(p + 8, r.right as u32)?;
    g.w32(p + 12, r.bottom as u32)
}

/// The value a handle-returning call puts in `rax`: HWNDs and friends are
/// 32-bit values, sign-extended on x86-64 as Windows does.
pub fn handle_ret(h: u32) -> u64 {
    h as i32 as i64 as u64
}

/// Handle one win32u syscall.
pub fn dispatch(c: &mut Call<'_>) -> R64 {
    use sysno::win32u as n;
    match c.nr {
        n::NtUserInitializeClientPfnArrays => class::nt_user_initialize_client_pfn_arrays(c),
        n::NtUserGetSystemDpiForProcess => sysparams::nt_user_get_system_dpi_for_process(c),
        n::NtUserGetProcessDpiAwarenessContext => sysparams::nt_user_get_process_dpi_awareness_context(c),
        n::NtUserSetProcessDpiAwarenessContext => sysparams::nt_user_set_process_dpi_awareness_context(c),
        n::NtUserSystemParametersInfo => sysparams::nt_user_system_parameters_info(c),
        n::NtUserSystemParametersInfoForDpi => sysparams::nt_user_system_parameters_info_for_dpi(c),
        n::NtUserGetProcessDefaultLayout => sysparams::nt_user_get_process_default_layout(c),
        n::NtUserSetProcessDefaultLayout => sysparams::nt_user_set_process_default_layout(c),
        n::NtUserCallNoParam => call_no_param(c),
        n::NtUserCallOneParam => call_one_param(c),
        n::NtUserCallTwoParam => call_two_param(c),
        n::NtUserCallHwnd => window::call_hwnd(c),
        n::NtUserCallHwndParam => window::call_hwnd_param(c),
        n::NtUserGetThreadState => message::nt_user_get_thread_state(c),
        n::NtUserRegisterClassExWOW => class::nt_user_register_class_ex_wow(c),
        n::NtUserUnregisterClass => class::nt_user_unregister_class(c),
        n::NtUserGetClassInfoEx => class::nt_user_get_class_info_ex(c),
        n::NtUserGetClassName => class::nt_user_get_class_name(c),
        n::NtUserSetClassLong => class::nt_user_set_class_long(c, 4),
        n::NtUserSetClassLongPtr => class::nt_user_set_class_long(c, c.p.abi.ptr_size() as u32),
        n::NtUserSetClassWord => class::nt_user_set_class_long(c, 2),
        n::NtUserRegisterWindowMessage => class::nt_user_register_window_message(c),
        n::NtUserGetAtomName => class::nt_user_get_atom_name(c),
        n::NtUserCreateWindowEx => window::nt_user_create_window_ex(c),
        n::NtUserDestroyWindow => window::nt_user_destroy_window(c),
        n::NtUserSetWindowLong => window::nt_user_set_window_long(c, 4),
        n::NtUserSetWindowLongPtr => window::nt_user_set_window_long(c, c.p.abi.ptr_size() as u32),
        n::NtUserSetWindowWord => window::nt_user_set_window_long(c, 2),
        n::NtUserGetProp => window::nt_user_get_prop(c),
        n::NtUserSetProp => window::nt_user_set_prop(c),
        n::NtUserRemoveProp => window::nt_user_remove_prop(c),
        n::NtUserBuildHwndList => window::nt_user_build_hwnd_list(c),
        n::NtUserGetAncestor => window::nt_user_get_ancestor(c),
        n::NtUserSetParent => window::nt_user_set_parent(c),
        n::NtUserFindWindowEx => window::nt_user_find_window_ex(c),
        n::NtUserInternalGetWindowText => window::nt_user_internal_get_window_text(c),
        n::NtUserSetWindowPos => window::nt_user_set_window_pos(c),
        n::NtUserMoveWindow => window::nt_user_move_window(c),
        n::NtUserShowWindow => window::nt_user_show_window(c),
        n::NtUserShowWindowAsync => window::nt_user_show_window(c),
        n::NtUserQueryWindow => window::nt_user_query_window(c),
        n::NtUserSetWindowFNID => window::nt_user_set_window_fnid(c),
        n::NtUserGetForegroundWindow => window::nt_user_get_foreground_window(c),
        n::NtUserSetForegroundWindow => window::nt_user_set_foreground_window(c),
        n::NtUserSetActiveWindow => window::nt_user_set_active_window(c),
        n::NtUserSetFocus => window::nt_user_set_focus(c),
        n::NtUserEnableWindow => window::nt_user_enable_window(c),
        n::NtUserGetWindowPlacement => window::nt_user_get_window_placement(c),
        n::NtUserMessageCall => message::nt_user_message_call(c),
        n::NtUserPostMessage => message::nt_user_post_message(c),
        n::NtUserPostThreadMessage => message::nt_user_post_thread_message(c),
        n::NtUserPostQuitMessage => message::nt_user_post_quit_message(c),
        n::NtUserPeekMessage => message::nt_user_peek_message(c),
        n::NtUserGetMessage => message::nt_user_get_message(c),
        n::NtUserDispatchMessage => message::nt_user_dispatch_message(c),
        n::NtUserTranslateMessage => message::nt_user_translate_message(c),
        n::NtUserWaitMessage => message::nt_user_wait_message(c),
        n::NtUserGetQueueStatus => message::nt_user_get_queue_status(c),
        n::NtUserMsgWaitForMultipleObjectsEx => message::nt_user_msg_wait_for_multiple_objects_ex(c),
        n::NtUserSetTimer => message::nt_user_set_timer(c, false),
        n::NtUserSetSystemTimer => message::nt_user_set_timer(c, true),
        n::NtUserKillTimer => message::nt_user_kill_timer(c, false),
        n::NtUserKillSystemTimer => message::nt_user_kill_timer(c, true),
        n::NtUserGetMessagePos => message::nt_user_get_message_pos(c),
        n::NtUserSetMessageExtraInfo => message::nt_user_set_message_extra_info(c),
        n::NtUserReplyMessage => message::nt_user_reply_message(c),
        n::NtUserNotifyWinEvent => Ok(0),
        n::NtUserEnumDisplayMonitors => sysparams::nt_user_enum_display_monitors(c),
        n::NtUserEnumDisplaySettings => sysparams::nt_user_enum_display_settings(c),
        n::NtUserEnumDisplayDevices => sysparams::nt_user_enum_display_devices(c),
        n::NtUserGetDC => paint::nt_user_get_dc(c),
        n::NtUserGetDCEx => paint::nt_user_get_dc_ex(c),
        n::NtUserGetWindowDC => paint::nt_user_get_window_dc(c),
        n::NtUserReleaseDC => paint::nt_user_release_dc(c),
        n::NtUserWindowFromDC => paint::nt_user_window_from_dc(c),
        n::NtUserBeginPaint => paint::nt_user_begin_paint(c),
        n::NtUserEndPaint => paint::nt_user_end_paint(c),
        n::NtUserInvalidateRect => paint::nt_user_invalidate_rect(c),
        n::NtUserValidateRect => paint::nt_user_validate_rect(c),
        n::NtUserInvalidateRgn => paint::nt_user_invalidate_rgn(c),
        n::NtUserValidateRgn => paint::nt_user_validate_rgn(c),
        n::NtUserGetUpdateRect => paint::nt_user_get_update_rect(c),
        n::NtUserGetUpdateRgn => paint::nt_user_get_update_rgn(c),
        n::NtUserRedrawWindow => paint::nt_user_redraw_window(c),
        n::NtUserExcludeUpdateRgn => paint::nt_user_exclude_update_rgn(c),
        nr if super::syscall_name(nr).starts_with("NtGdi") => super::gdi::dispatch(c),
        nr => Err(c.unimplemented(&format!("win32u {} ({nr:#x})", super::syscall_name(nr)))),
    }
}

/// `NtUserCallNoParam(code)`.
fn call_no_param(c: &mut Call<'_>) -> R64 {
    let code = c.a32(0);
    match code {
        // GetDesktopWindow
        0 => Ok(handle_ret(window::get_desktop_window(c)?)),
        // GetDialogBaseUnits: from the system font (8x16 Tahoma 8pt, as Wine).
        1 => Ok(0x0010_0008),
        // GetLastInputTime
        2 => Ok(message::tick_count(c) as u64),
        // GetProgmanWindow, GetShellWindow, GetTaskmanWindow: none.
        3..=5 => Ok(0),
        // NtUserExitingThread, NtUserThreadDetach: the thread's windows and
        // queue go with it.
        7 | 8 => {
            window::destroy_thread_windows(c)?;
            let tid = c.t.tid;
            c.p.user.lock().unwrap().threads.remove(&tid);
            Ok(0)
        }
        _ => Err(c.unimplemented(&format!("NtUserCallNoParam code {code}"))),
    }
}

/// `NtUserCallOneParam(arg, code)`.
fn call_one_param(c: &mut Call<'_>) -> R64 {
    let (arg, code) = (c.a(0), c.a32(1));
    match code {
        // GetPrimaryMonitorRect
        5 => {
            let r = sysparams::primary_rect(c);
            write_rect(&mut c.g, arg, &r)?;
            Ok(1)
        }
        // GetSysColor
        6 => Ok(sysparams::sys_color(arg as i32) as u64),
        // GetSysColorBrush
        7 => Ok(handle_ret(crate::nt::gdi::sys_color_brush(c, arg as u32)?)),
        // GetSystemMetrics
        9 => Ok(sysparams::system_metrics(c, arg as i32) as i32 as i64 as u64),
        // SetThreadDpiAwarenessContext
        12 => Ok(sysparams::set_thread_dpi_awareness_context(c, arg as u32)? as u64),
        _ => Err(c.unimplemented(&format!("NtUserCallOneParam code {code}"))),
    }
}

/// `NtUserCallTwoParam(arg1, arg2, code)`.
fn call_two_param(c: &mut Call<'_>) -> R64 {
    let (a1, a2, code) = (c.a(0), c.a(1), c.a32(2));
    match code {
        // GetMonitorInfo
        2 => sysparams::get_monitor_info(c, a1 as u32, a2),
        // GetSystemMetricsForDpi
        3 => Ok(sysparams::system_metrics(c, a1 as i32) as i32 as i64 as u64),
        // MonitorFromRect
        4 => Ok(handle_ret(sysparams::MONITOR)),
        // AdjustWindowRect: (RECT *, adjust_window_rect_params *)
        7 => {
            let style = c.g.u32(a2)?;
            let ex_style = c.g.u32(a2 + 4)?;
            let menu = c.g.u32(a2 + 8)? != 0;
            let mut r = read_rect(&c.g, a1)?;
            window::adjust_window_rect(&mut r, style, menu, ex_style);
            write_rect(&mut c.g, a1, &r)?;
            Ok(1)
        }
        // GetVirtualScreenRect
        8 => {
            let r = sysparams::primary_rect(c);
            write_rect(&mut c.g, a1, &r)?;
            Ok(1)
        }
        // NtUserAllocWinProc (temporary export): (func, ansi)
        9 => Ok(class::alloc_winproc(c, a1, a2 != 0)),
        _ => Err(c.unimplemented(&format!("NtUserCallTwoParam code {code}"))),
    }
}

#[allow(dead_code)]
fn _unused(_: Abi, _: VecDeque<u8>) -> u32 {
    st::SUCCESS
}
