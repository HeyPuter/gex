//! Window classes, winprocs and user atoms (win32u's class.c and
//! wineserver's class.c).

use super::{err, handle_ret, read_name, set_last_error, window, NameOrAtom, User, NB_PROCS, NB_WORKERS, R64};
use crate::nt::{off, Call, Status, R};

/// `MAKE_WNDPROC(index)`: a winproc handle, `0xffff0000 | index`.
pub fn make_wndproc(index: usize) -> u64 {
    0xffff_0000 | index as u64
}

/// The winproc table index a handle names, if it is one.
pub fn winproc_index(u: &User, h: u64) -> Option<usize> {
    let i = (h & 0xffff) as usize;
    (h == make_wndproc(i) && i < u.winprocs.len()).then_some(i)
}

/// A window class.
#[derive(Clone, Debug)]
pub struct Class {
    /// Stable id, which windows refer to their class by.
    pub id: u32,
    /// The class atom (of the base name).
    pub atom: u16,
    /// The full name as registered (with any version prefix).
    pub name: Vec<u16>,
    /// Where the base name starts in `name`.
    pub name_offset: usize,
    pub local: bool,
    pub instance: u64,
    pub style: u32,
    /// A winproc handle (`MAKE_WNDPROC`) or a raw client pointer.
    pub wndproc: u64,
    pub cls_extra: u32,
    pub win_extra: u32,
    pub icon: u64,
    pub icon_small: u64,
    pub cursor: u64,
    pub background: u64,
    /// user32's `struct client_menu_name *`.
    pub menu_name: u64,
    pub fnid: u16,
    pub extra: Vec<u8>,
    /// Windows of this class, which keep it from being unregistered.
    pub windows: u32,
}

/// Allocate a winproc handle for a client window procedure (Wine's
/// `alloc_winproc`). Never freed, as in Wine.
pub fn alloc_winproc_locked(u: &mut User, func: u64, ansi: bool) -> u64 {
    if func == 0 {
        return 0;
    }
    if winproc_index(u, func).is_some() {
        return func;
    }
    // Builtins match either proc (some programs confuse A and W).
    for (i, &(a, w)) in u.winprocs.iter().enumerate().take(NB_PROCS) {
        if a == func || w == func {
            return make_wndproc(i);
        }
    }
    for (i, &(a, w)) in u.winprocs.iter().enumerate().skip(NB_PROCS) {
        if (ansi && a == func) || (!ansi && w == func) {
            return make_wndproc(i);
        }
    }
    if u.winprocs.len() >= 4096 {
        return func;
    }
    u.winprocs.push(if ansi { (func, 0) } else { (0, func) });
    make_wndproc(u.winprocs.len() - 1)
}

pub fn alloc_winproc(c: &Call<'_>, func: u64, ansi: bool) -> u64 {
    alloc_winproc_locked(&mut c.p.user.lock().unwrap(), func, ansi)
}

/// A pointer a program may call (Wine's `get_winproc`).
pub fn get_winproc(u: &User, proc_: u64, ansi: bool) -> u64 {
    match winproc_index(u, proc_) {
        None => proc_,
        Some(i) => {
            let (a, w) = u.winprocs[i];
            let p = if ansi { a } else { w };
            if p != 0 {
                p
            } else {
                proc_
            }
        }
    }
}

/// A dialog procedure handle's procedure for `ansi` (Wine's
/// `get_dialog_proc`): unlike [`get_winproc`], 0 when it has none of that
/// kind, so user32 tries the other.
pub fn get_dialog_proc(u: &User, proc_: u64, ansi: bool) -> u64 {
    match winproc_index(u, proc_) {
        None => proc_,
        Some(i) => {
            let (a, w) = u.winprocs[i];
            if ansi {
                a
            } else {
                w
            }
        }
    }
}

/// `NtUserInitializeClientPfnArrays(procsA, procsW, workers, user32)`: the
/// entries are 64-bit on every ABI (`ntuser_client_func_ptr`), a 32-bit
/// process's pointer in the low half.
pub fn nt_user_initialize_client_pfn_arrays(c: &mut Call<'_>) -> R64 {
    let (pa, pw, workers, module) = (c.a(0), c.a(1), c.a(2), c.a(3));
    let ps = 8;
    let mut procs = vec![(0u64, 0u64); NB_PROCS];
    for (i, p) in procs.iter_mut().enumerate() {
        *p = (c.g.ptr(pa + ps * i as u64)?, c.g.ptr(pw + ps * i as u64)?);
    }
    let mut w = [0u64; NB_WORKERS];
    for (i, v) in w.iter_mut().enumerate() {
        *v = c.g.ptr(workers + ps * i as u64)?;
    }
    let mut u = c.p.user.lock().unwrap();
    u.winprocs[..NB_PROCS].copy_from_slice(&procs);
    u.workers = w;
    u.user32 = module;
    u.initialized = true;
    drop(u);
    // win32u's init_user sets up GDI here too (gdi_init).
    crate::nt::gdi::ensure_init(c)?;
    Ok(0)
}

/// The class a name (or atom) names for `instance` (wineserver's
/// `find_class`).
pub fn find_class(u: &User, name: &NameOrAtom, instance: u64) -> Option<usize> {
    let atom = match name {
        NameOrAtom::Atom(a) => *a,
        NameOrAtom::Name(n) => u.find_atom(n)?,
    };
    u.classes.iter().position(|cl| {
        if cl.atom != atom && !matches!(name, NameOrAtom::Name(n) if super::lower(n) == super::lower(&cl.name)) {
            return false;
        }
        let win16 = cl.instance >> 16 == 0;
        instance == 0
            || !cl.local
            || cl.instance == instance
            || (!win16 && (cl.instance & !0xffff) == (instance & !0xffff))
    })
}

/// Register a class. Returns its atom, or 0 with the last error set.
#[allow(clippy::too_many_arguments)]
pub fn register_class(
    u: &mut User,
    name: &NameOrAtom,
    name_offset: usize,
    instance: u64,
    style: u32,
    wndproc: u64,
    cls_extra: i32,
    win_extra: i32,
    icon: u64,
    icon_small: u64,
    cursor: u64,
    background: u64,
    menu_name: u64,
    fnid: u16,
) -> Result<u16, u32> {
    const CS_GLOBALCLASS: u32 = 0x4000;
    if cls_extra < 0 || win_extra < 0 || cls_extra > 4096 || win_extra > 4096 {
        return Err(err::INVALID_PARAMETER);
    }
    let (full, atom) = match name {
        NameOrAtom::Atom(a) => (u.atom_name(*a).unwrap_or_default(), *a),
        NameOrAtom::Name(n) => (n.clone(), 0),
    };
    let base: Vec<u16> = if name_offset > 0 && name_offset < full.len() { full[name_offset..].to_vec() } else { full.clone() };
    let base_atom = if atom != 0 && name_offset == 0 { atom } else { u.add_atom(&base) };
    let local = fnid == 0 && style & CS_GLOBALCLASS == 0;
    if let Some(i) = find_class(u, &NameOrAtom::Atom(base_atom), instance) {
        if u.classes[i].local == local {
            return Err(err::CLASS_ALREADY_EXISTS);
        }
    }
    u.next_class_id += 1;
    let class = Class {
        id: u.next_class_id,
        atom: base_atom,
        name: full,
        name_offset: if name_offset < base.len() + name_offset { name_offset } else { 0 },
        local,
        instance,
        style,
        wndproc,
        cls_extra: cls_extra as u32,
        win_extra: win_extra as u32,
        icon,
        icon_small,
        cursor,
        background,
        menu_name,
        fnid,
        extra: vec![0; cls_extra as usize],
        windows: 0,
    };
    // Local classes are searched first, as Wine keeps them at the head.
    if local {
        u.classes.insert(0, class);
    } else {
        u.classes.push(class);
    }
    Ok(base_atom)
}

/// `NtUserRegisterClassExWOW(wc, name, version, menu_name, fnid, flags, wow)`.
pub fn nt_user_register_class_ex_wow(c: &mut Call<'_>) -> R64 {
    let (wc, name_p, version_p, menu_name, fnid, ansi) = (c.a(0), c.a(1), c.a(2), c.a(3), c.a32(4), c.a32(5) != 0);
    // Creating the desktop window registers the builtin classes first.
    if fnid == 0 {
        window::get_desktop_window(c)?;
    }
    let abi = c.p.abi;
    macro_rules! f {
        ($name:ident) => {
            wc + off!(abi, wndclassexw::$name)
        };
    }
    let size = c.g.u32(f!(CBSIZE))?;
    let instance = c.g.ptr(f!(HINSTANCE))?;
    let user32 = c.p.user.lock().unwrap().user32;
    if size as u64 != off!(abi, wndclassexw::SIZEOF) || (fnid == 0 && instance == user32 && instance != 0) {
        set_last_error(c, err::INVALID_PARAMETER);
        return Ok(0);
    }
    let instance = if instance == 0 { c.g.ptr(c.p.peb.load(std::sync::atomic::Ordering::Relaxed) + off!(abi, peb::IMAGEBASEADDRESS))? } else { instance };
    let name = read_name(&c.g, name_p)?.ok_or(Status(crate::nt::st::INVALID_PARAMETER))?;
    let version_len = if version_p != 0 { c.g.u16(version_p)? as usize / 2 } else { 0 };
    let style = c.g.u32(f!(STYLE))?;
    let proc_ = c.g.ptr(f!(LPFNWNDPROC))?;
    let cls_extra = c.g.u32(f!(CBCLSEXTRA))? as i32;
    let win_extra = c.g.u32(f!(CBWNDEXTRA))? as i32;
    let icon = c.g.ptr(f!(HICON))?;
    let icon_small = c.g.ptr(f!(HICONSM))?;
    let cursor = c.g.ptr(f!(HCURSOR))?;
    let background = c.g.ptr(f!(HBRBACKGROUND))?;
    let mut u = c.p.user.lock().unwrap();
    let wndproc = alloc_winproc_locked(&mut u, proc_, ansi);
    let r = register_class(
        &mut u, &name, version_len, instance, style, wndproc, cls_extra, win_extra, icon, icon_small, cursor,
        background, menu_name, fnid as u16,
    );
    drop(u);
    match r {
        Ok(atom) => Ok(atom as u64),
        Err(e) => {
            set_last_error(c, e);
            Ok(0)
        }
    }
}

/// `NtUserUnregisterClass(name, instance, *menu_name)`.
pub fn nt_user_unregister_class(c: &mut Call<'_>) -> R64 {
    let (name_p, instance, menu_p) = (c.a(0), c.a(1), c.a(2));
    window::get_desktop_window(c)?;
    let name = read_name(&c.g, name_p)?.ok_or(Status(crate::nt::st::INVALID_PARAMETER))?;
    let mut u = c.p.user.lock().unwrap();
    let Some(i) = find_class(&u, &name, instance) else {
        drop(u);
        set_last_error(c, err::CLASS_DOES_NOT_EXIST);
        return Ok(0);
    };
    if u.classes[i].windows > 0 {
        drop(u);
        set_last_error(c, err::CLASS_HAS_WINDOWS);
        return Ok(0);
    }
    let cl = u.classes.remove(i);
    drop(u);
    if menu_p != 0 {
        c.g.wptr(menu_p, cl.menu_name)?;
    }
    Ok(1)
}

/// `NtUserGetClassInfoEx(instance, name, *wc, *menu_name, ansi)`.
pub fn nt_user_get_class_info_ex(c: &mut Call<'_>) -> R64 {
    let (instance, name_p, wc, menu_p, ansi) = (c.a(0), c.a(1), c.a(2), c.a(3), c.a32(4) != 0);
    let name = read_name(&c.g, name_p)?.ok_or(Status(crate::nt::st::INVALID_PARAMETER))?;
    let is_system = matches!(&name, NameOrAtom::Name(n) if matches!(crate::nt::wstr(n).to_ascii_lowercase().as_str(), "#32769" | "message"));
    if !is_system {
        window::get_desktop_window(c)?;
    }
    let u = c.p.user.lock().unwrap();
    let Some(i) = find_class(&u, &name, instance) else {
        drop(u);
        set_last_error(c, err::CLASS_DOES_NOT_EXIST);
        return Ok(0);
    };
    let cl = u.classes[i].clone();
    let proc_ = get_winproc(&u, cl.wndproc, ansi);
    let user32 = u.user32;
    drop(u);
    if wc != 0 {
        let abi = c.p.abi;
        macro_rules! f {
            ($name:ident) => {
                wc + off!(abi, wndclassexw::$name)
            };
        }
        c.g.w32(f!(STYLE), cl.style)?;
        c.g.wptr(f!(LPFNWNDPROC), proc_)?;
        c.g.w32(f!(CBCLSEXTRA), cl.cls_extra)?;
        c.g.w32(f!(CBWNDEXTRA), cl.win_extra)?;
        c.g.wptr(f!(HINSTANCE), if instance == user32 { 0 } else { instance })?;
        c.g.wptr(f!(HICON), cl.icon)?;
        c.g.wptr(f!(HICONSM), cl.icon_small)?;
        c.g.wptr(f!(HCURSOR), cl.cursor)?;
        c.g.wptr(f!(HBRBACKGROUND), cl.background)?;
        c.g.wptr(f!(LPSZMENUNAME), cl.menu_name)?;
        let buf = if name_p != 0 { c.g.ptr(name_p + off!(abi, unicode_string::BUFFER))? } else { 0 };
        c.g.wptr(f!(LPSZCLASSNAME), buf)?;
        if menu_p != 0 {
            c.g.wptr(menu_p, cl.menu_name)?;
        }
    }
    Ok(cl.atom as u64)
}

/// `NtUserGetClassName(hwnd, real, *name)`.
pub fn nt_user_get_class_name(c: &mut Call<'_>) -> R64 {
    let (hwnd, _real, us) = (c.a32(0), c.a32(1), c.a(2));
    let max = c.g.u16(us + 2)? as usize;
    if max <= 2 {
        set_last_error(c, err::INSUFFICIENT_BUFFER);
        return Ok(0);
    }
    let u = c.p.user.lock().unwrap();
    let Some(w) = window::find(&u, hwnd) else {
        drop(u);
        set_last_error(c, err::INVALID_WINDOW_HANDLE);
        return Ok(0);
    };
    let cl = window::class_of(&u, w);
    let base: Vec<u16> = cl.name[cl.name_offset.min(cl.name.len())..].to_vec();
    drop(u);
    let n = base.len().min(max / 2 - 1);
    let buf = c.g.ptr(us + off!(c.p.abi, unicode_string::BUFFER))?;
    let mut z = base[..n].to_vec();
    z.push(0);
    c.g.write_wchars(buf, &z)?;
    Ok(n as u64)
}

/// `GetClassLong*` (through `NtUserCallHwndParam`).
pub fn get_class_long(c: &mut Call<'_>, hwnd: u32, offset: i32, size: u32, ansi: bool) -> R64 {
    const GCLP_MENUNAME: i32 = -8;
    const GCLP_HBRBACKGROUND: i32 = -10;
    const GCLP_HCURSOR: i32 = -12;
    const GCLP_HICON: i32 = -14;
    const GCLP_HMODULE: i32 = -16;
    const GCL_CBWNDEXTRA: i32 = -18;
    const GCL_CBCLSEXTRA: i32 = -20;
    const GCLP_WNDPROC: i32 = -24;
    const GCL_STYLE: i32 = -26;
    const GCW_ATOM: i32 = -32;
    const GCLP_HICONSM: i32 = -34;
    let u = c.p.user.lock().unwrap();
    let Some(w) = window::find(&u, hwnd) else {
        drop(u);
        set_last_error(c, err::INVALID_WINDOW_HANDLE);
        return Ok(0);
    };
    let cl = window::class_of(&u, w);
    let v = match offset {
        GCW_ATOM => cl.atom as u64,
        GCL_STYLE => cl.style as u64,
        GCLP_WNDPROC => get_winproc(&u, cl.wndproc, ansi),
        GCL_CBCLSEXTRA => cl.cls_extra as u64,
        GCL_CBWNDEXTRA => cl.win_extra as u64,
        GCLP_HMODULE => cl.instance,
        GCLP_HICON => cl.icon,
        GCLP_HICONSM => cl.icon_small,
        GCLP_HCURSOR => cl.cursor,
        GCLP_HBRBACKGROUND => cl.background,
        GCLP_MENUNAME => cl.menu_name,
        o if o >= 0 && (o as u32 + size) <= cl.cls_extra => {
            let mut b = [0u8; 8];
            b[..size as usize].copy_from_slice(&cl.extra[o as usize..o as usize + size as usize]);
            u64::from_le_bytes(b)
        }
        _ => {
            drop(u);
            set_last_error(c, err::INVALID_INDEX);
            return Ok(0);
        }
    };
    Ok(v)
}

/// `NtUserSetClassLong(Ptr)`/`NtUserSetClassWord(hwnd, offset, value[, ansi])`.
pub fn nt_user_set_class_long(c: &mut Call<'_>, size: u32) -> R64 {
    let (hwnd, offset, value) = (c.a32(0), c.a32(1) as i32, c.a(2));
    let ansi = if size == 2 { true } else { c.a32(3) != 0 };
    let mut u = c.p.user.lock().unwrap();
    let Some(ci) = window::find(&u, hwnd).and_then(|w| u.classes.iter().position(|cl| cl.id == w.class)) else {
        drop(u);
        set_last_error(c, err::INVALID_WINDOW_HANDLE);
        return Ok(0);
    };
    let new_proc = if offset == -24 { alloc_winproc_locked(&mut u, value, ansi) } else { 0 };
    let cl = &mut u.classes[ci];
    let old = match offset {
        -26 => std::mem::replace(&mut cl.style, value as u32) as u64,
        -24 => std::mem::replace(&mut cl.wndproc, new_proc),
        -18 => std::mem::replace(&mut cl.win_extra, value as u32) as u64,
        -16 => std::mem::replace(&mut cl.instance, value),
        -14 => std::mem::replace(&mut cl.icon, value),
        -34 => std::mem::replace(&mut cl.icon_small, value),
        -12 => std::mem::replace(&mut cl.cursor, value),
        -10 => std::mem::replace(&mut cl.background, value),
        -8 => std::mem::replace(&mut cl.menu_name, value),
        o if o >= 0 && (o as u32 + size) <= cl.cls_extra => {
            let r = o as usize..o as usize + size as usize;
            let mut b = [0u8; 8];
            b[..size as usize].copy_from_slice(&cl.extra[r.clone()]);
            cl.extra[r].copy_from_slice(&value.to_le_bytes()[..size as usize]);
            u64::from_le_bytes(b)
        }
        _ => {
            drop(u);
            set_last_error(c, if offset == -20 { err::INVALID_PARAMETER } else { err::INVALID_INDEX });
            return Ok(0);
        }
    };
    let old = if offset == -24 { get_winproc(&u, old, ansi) } else { old };
    Ok(old)
}

/// `NtUserRegisterWindowMessage(name)`.
pub fn nt_user_register_window_message(c: &mut Call<'_>) -> R64 {
    let p = c.a(0);
    match read_name(&c.g, p)? {
        None => {
            set_last_error(c, err::INVALID_PARAMETER);
            Ok(0)
        }
        Some(NameOrAtom::Atom(a)) => Ok(a as u64),
        Some(NameOrAtom::Name(n)) => {
            if n.is_empty() || n.len() > 255 {
                set_last_error(c, err::INVALID_PARAMETER);
                return Ok(0);
            }
            Ok(c.p.user.lock().unwrap().add_atom(&n) as u64)
        }
    }
}

/// `NtUserGetAtomName(atom, *name)`: characters copied.
pub fn nt_user_get_atom_name(c: &mut Call<'_>) -> R64 {
    let (atom, us) = (c.a32(0) as u16, c.a(1));
    let Some(name) = c.p.user.lock().unwrap().atom_name(atom) else {
        set_last_error(c, err::INVALID_PARAMETER);
        return Ok(0);
    };
    let max = c.g.u16(us + 2)? as usize;
    if max < 2 {
        set_last_error(c, err::INSUFFICIENT_BUFFER);
        return Ok(0);
    }
    let n = name.len().min(max / 2 - 1);
    let buf = c.g.ptr(us + off!(c.p.abi, unicode_string::BUFFER))?;
    let mut z = name[..n].to_vec();
    z.push(0);
    c.g.write_wchars(buf, &z)?;
    Ok(n as u64)
}

/// The builtin classes win32u registers when the desktop window is created
/// (Wine's `builtin_classes`): (proc, name, style, window extra bytes,
/// background). Cursors are left unset for now (no cursor objects yet).
pub fn builtin_classes(p64: bool) -> Vec<(usize, &'static str, u32, u32, u64)> {
    use super::procs::*;
    let ps = if p64 { 8 } else { 4 };
    const CS_DBLCLKS: u32 = 0x8;
    const CS_VREDRAW: u32 = 0x1;
    const CS_HREDRAW: u32 = 0x2;
    const CS_PARENTDC: u32 = 0x80;
    const CS_SAVEBITS: u32 = 0x800;
    const CS_DROPSHADOW: u32 = 0x20000;
    vec![
        (SCROLLBAR, "ScrollBar", CS_DBLCLKS | CS_VREDRAW | CS_HREDRAW | CS_PARENTDC, ps, 0),
        (MESSAGE, "Message", 0, 0, 0),
        (MENU, "#32768", CS_DROPSHADOW | CS_SAVEBITS | CS_DBLCLKS, ps, 5), // COLOR_MENU + 1
        (DESKTOP, "#32769", CS_DBLCLKS, 0, 2),                              // COLOR_BACKGROUND + 1
        (ICONTITLE, "#32772", 0, 0, 0),
        (BUTTON, "Button", CS_DBLCLKS | CS_VREDRAW | CS_HREDRAW | CS_PARENTDC, 4 + 2 * ps, 0),
        (COMBO, "ComboBox", CS_PARENTDC | CS_DBLCLKS | CS_HREDRAW | CS_VREDRAW, ps, 0),
        (COMBOLBOX, "ComboLBox", CS_DBLCLKS | CS_SAVEBITS, ps, 0),
        (DIALOG, "#32770", CS_SAVEBITS | CS_DBLCLKS, if p64 { 48 } else { 30 }, 0), // DLGWINDOWEXTRA
        (EDIT, "Edit", CS_DBLCLKS | CS_PARENTDC, if p64 { 8 } else { 6 }, 0),
        (LISTBOX, "ListBox", CS_DBLCLKS, ps, 0),
        (MDICLIENT, "MDIClient", 0, 2 * ps, 13), // COLOR_APPWORKSPACE + 1
        (STATIC, "Static", CS_DBLCLKS | CS_PARENTDC, 2 * ps, 0),
        (IME, "IME", 0, 2 * ps, 0),
    ]
}

/// Register one builtin class (Wine's `register_builtin`).
pub fn register_builtin(u: &mut User, p64: bool, proc_: usize) {
    if u.builtin_atoms[proc_] != 0 {
        return;
    }
    let Some(&(_, name, style, extra, brush)) = builtin_classes(p64).iter().find(|b| b.0 == proc_) else { return };
    let instance = u.user32;
    let wndproc = make_wndproc(proc_);
    let n = NameOrAtom::Name(crate::nt::to_w(name)).normalize();
    // "#32769" and friends are integer atoms.
    let n = match n {
        NameOrAtom::Atom(a) => {
            // Integer atoms need a name entry for GetClassName; they keep
            // their numeric atom.
            let _ = a;
            NameOrAtom::Atom(a)
        }
        other => other,
    };
    if let Ok(atom) = register_class(
        u,
        &n,
        0,
        instance,
        style,
        wndproc,
        0,
        extra as i32,
        0,
        0,
        0,
        brush,
        0,
        0x8000 | proc_ as u16,
    ) {
        u.builtin_atoms[proc_] = atom;
    }
}

#[allow(dead_code)]
fn _unused(_: R<()>) -> u64 {
    handle_ret(0)
}
