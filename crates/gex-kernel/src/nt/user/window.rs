//! The window tree: creation and destruction, positions and visibility,
//! window data, properties, and the `NtUserCallHwnd*` families (win32u's
//! window.c, winpos.c parts and wineserver's window.c).
//!
//! Rectangles are kept as Wine keeps them: the window and client rectangles
//! in the parent's client coordinates.

use super::winproc::{self, Pack};
use super::{class, err, handle_ret, read_name, read_rect, set_last_error, write_rect, NameOrAtom, ObjKind, User, R64};
use crate::nt::{off, Call, Status, R};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
    pub fn offset(&self, dx: i32, dy: i32) -> Rect {
        Rect { left: self.left + dx, top: self.top + dy, right: self.right + dx, bottom: self.bottom + dy }
    }
    pub fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }
    pub fn intersect(&self, o: &Rect) -> Rect {
        let r = Rect {
            left: self.left.max(o.left),
            top: self.top.max(o.top),
            right: self.right.min(o.right),
            bottom: self.bottom.min(o.bottom),
        };
        if r.is_empty() {
            Rect::default()
        } else {
            r
        }
    }
    pub fn union(&self, o: &Rect) -> Rect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        Rect {
            left: self.left.min(o.left),
            top: self.top.min(o.top),
            right: self.right.max(o.right),
            bottom: self.bottom.max(o.bottom),
        }
    }
}

pub mod ws {
    pub const OVERLAPPED: u32 = 0;
    pub const POPUP: u32 = 0x8000_0000;
    pub const CHILD: u32 = 0x4000_0000;
    pub const MINIMIZE: u32 = 0x2000_0000;
    pub const VISIBLE: u32 = 0x1000_0000;
    pub const DISABLED: u32 = 0x0800_0000;
    pub const CLIPSIBLINGS: u32 = 0x0400_0000;
    pub const CLIPCHILDREN: u32 = 0x0200_0000;
    pub const MAXIMIZE: u32 = 0x0100_0000;
    pub const CAPTION: u32 = 0x00c0_0000;
    pub const BORDER: u32 = 0x0080_0000;
    pub const DLGFRAME: u32 = 0x0040_0000;
    pub const VSCROLL: u32 = 0x0020_0000;
    pub const HSCROLL: u32 = 0x0010_0000;
    pub const SYSMENU: u32 = 0x0008_0000;
    pub const THICKFRAME: u32 = 0x0004_0000;
    pub const MINIMIZEBOX: u32 = 0x0002_0000;
    pub const MAXIMIZEBOX: u32 = 0x0001_0000;
    pub mod ex {
        pub const DLGMODALFRAME: u32 = 0x1;
        pub const NOPARENTNOTIFY: u32 = 0x4;
        pub const TOPMOST: u32 = 0x8;
        pub const TOOLWINDOW: u32 = 0x80;
        pub const WINDOWEDGE: u32 = 0x100;
        pub const CLIENTEDGE: u32 = 0x200;
        pub const STATICEDGE: u32 = 0x20000;
        pub const APPWINDOW: u32 = 0x40000;
        pub const LAYERED: u32 = 0x80000;
        pub const LAYOUTRTL: u32 = 0x400000;
        pub const MDICHILD: u32 = 0x40;
    }
}

pub mod swp {
    pub const NOSIZE: u32 = 0x1;
    pub const NOMOVE: u32 = 0x2;
    pub const NOZORDER: u32 = 0x4;
    pub const NOREDRAW: u32 = 0x8;
    pub const NOACTIVATE: u32 = 0x10;
    pub const FRAMECHANGED: u32 = 0x20;
    pub const SHOWWINDOW: u32 = 0x40;
    pub const HIDEWINDOW: u32 = 0x80;
    pub const NOCOPYBITS: u32 = 0x100;
    pub const NOOWNERZORDER: u32 = 0x200;
    pub const NOSENDCHANGING: u32 = 0x400;
    pub const DEFERERASE: u32 = 0x2000;
    pub const ASYNCWINDOWPOS: u32 = 0x4000;
    pub const STATECHANGED: u32 = 0x8000;
}

pub mod wm {
    pub const CREATE: u32 = 0x0001;
    pub const DESTROY: u32 = 0x0002;
    pub const MOVE: u32 = 0x0003;
    pub const SIZE: u32 = 0x0005;
    pub const ACTIVATE: u32 = 0x0006;
    pub const SETFOCUS: u32 = 0x0007;
    pub const KILLFOCUS: u32 = 0x0008;
    pub const ENABLE: u32 = 0x000a;
    pub const SETTEXT: u32 = 0x000c;
    pub const GETTEXT: u32 = 0x000d;
    pub const GETTEXTLENGTH: u32 = 0x000e;
    pub const PAINT: u32 = 0x000f;
    pub const CLOSE: u32 = 0x0010;
    pub const QUIT: u32 = 0x0012;
    pub const ERASEBKGND: u32 = 0x0014;
    pub const SHOWWINDOW: u32 = 0x0018;
    pub const ACTIVATEAPP: u32 = 0x001c;
    pub const SETCURSOR: u32 = 0x0020;
    pub const MOUSEACTIVATE: u32 = 0x0021;
    pub const GETMINMAXINFO: u32 = 0x0024;
    pub const WINDOWPOSCHANGING: u32 = 0x0046;
    pub const WINDOWPOSCHANGED: u32 = 0x0047;
    pub const STYLECHANGING: u32 = 0x007c;
    pub const STYLECHANGED: u32 = 0x007d;
    pub const NCCREATE: u32 = 0x0081;
    pub const NCDESTROY: u32 = 0x0082;
    pub const NCCALCSIZE: u32 = 0x0083;
    pub const NCHITTEST: u32 = 0x0084;
    pub const NCPAINT: u32 = 0x0085;
    pub const NCACTIVATE: u32 = 0x0086;
    pub const PARENTNOTIFY: u32 = 0x0210;
    pub const TIMER: u32 = 0x0113;
    pub const SYSTIMER: u32 = 0x0118;
    pub const USER: u32 = 0x0400;
}

/// `WIN_NEED_SIZE`: the window still owes its WM_SIZE/WM_MOVE.
pub const WIN_NEED_SIZE: u32 = 0x4;

/// A window property: a string (as an atom) or an integer atom.
#[derive(Clone, Debug)]
pub struct Prop {
    pub atom: u16,
    pub string: bool,
    pub data: u64,
}

#[derive(Clone, Debug)]
pub struct Window {
    pub hwnd: u32,
    pub parent: u32,
    pub owner: u32,
    /// Z order, top first.
    pub children: Vec<u32>,
    /// The class's id ([`class::Class::id`]).
    pub class: u32,
    pub style: u32,
    pub ex_style: u32,
    pub id: u64,
    pub instance: u64,
    pub user_data: u64,
    /// A winproc handle (`MAKE_WNDPROC`) or a raw client pointer.
    pub wndproc: u64,
    pub unicode: bool,
    pub text: Vec<u16>,
    pub window_rect: Rect,
    pub client_rect: Rect,
    pub normal_rect: Rect,
    pub extra: Vec<u8>,
    pub props: Vec<Prop>,
    pub tid: u32,
    pub flags: u32,
    pub fnid: u16,
    pub dpi_context: u32,
    pub menu: u64,
    pub destroying: bool,
    /// What needs repainting, in client coordinates (empty: nothing), and
    /// whether WM_ERASEBKGND is still owed for it ([`super::paint`]).
    pub update: Rect,
    pub erase: bool,
    /// RedrawWindow(RDW_INTERNALPAINT): a WM_PAINT without an update region.
    pub internal_paint: bool,
    /// A top-level window's surface: a GDI bitmap handle, 0 until drawn to.
    pub surface: u32,
    /// The window's own DC (CS_OWNDC), 0 until asked for.
    pub own_dc: u32,
    /// user32's `DIALOGINFO` for a dialog (SetDialogInfo).
    pub dlg_info: u64,
    /// A builtin control's state (NtUserSetPrivateData), grown as written.
    pub private: Vec<u8>,
    /// The part of the window a swapchain presents (D3DKMT's
    /// SET_PRESENT_RECT_WINE escape); empty for the whole client area.
    pub present_rect: Rect,
}

/// The window `hwnd` names (a full or a 16-bit handle).
pub fn find(u: &User, hwnd: u32) -> Option<&Window> {
    let full = u.full_handle(hwnd, ObjKind::Window)?;
    u.windows.get(&full)
}

pub fn find_mut(u: &mut User, hwnd: u32) -> Option<&mut Window> {
    let full = u.full_handle(hwnd, ObjKind::Window)?;
    u.windows.get_mut(&full)
}

/// The full handle, or 0.
pub fn full(u: &User, hwnd: u32) -> u32 {
    u.full_handle(hwnd, ObjKind::Window).filter(|h| u.windows.contains_key(h)).unwrap_or(0)
}

/// A window's class.
pub fn class_of<'a>(u: &'a User, w: &Window) -> &'a class::Class {
    u.classes.iter().find(|cl| cl.id == w.class).expect("a window's class is always registered")
}

/// The origin of `hwnd`'s client area in screen coordinates.
pub fn client_origin(u: &User, hwnd: u32) -> (i32, i32) {
    let (mut x, mut y) = (0, 0);
    let mut h = hwnd;
    while let Some(w) = u.windows.get(&h) {
        if h == u.desktop {
            break;
        }
        x += w.client_rect.left;
        y += w.client_rect.top;
        h = w.parent;
    }
    (x, y)
}

/// A window's rectangle in screen coordinates.
pub fn screen_window_rect(u: &User, w: &Window) -> Rect {
    let (x, y) = client_origin(u, w.parent);
    w.window_rect.offset(x, y)
}

pub fn screen_client_rect(u: &User, w: &Window) -> Rect {
    let (x, y) = client_origin(u, w.parent);
    w.client_rect.offset(x, y)
}

/// Wine's `adjust_window_rect` with Wine's default metrics: the window
/// rectangle that has `r` as its client area.
pub fn adjust_window_rect(r: &mut Rect, style: u32, menu: bool, ex_style: u32) {
    use ws::ex;
    let mut adjust = 0;
    if ex_style & (ex::STATICEDGE | ex::DLGMODALFRAME) == ex::STATICEDGE {
        adjust = 1;
    } else if ex_style & ex::DLGMODALFRAME != 0 || style & (ws::THICKFRAME | ws::DLGFRAME) != 0 {
        adjust = 2;
    }
    if style & ws::THICKFRAME != 0 {
        adjust += 1; // iBorderWidth + iPaddedBorderWidth
    }
    if style & (ws::BORDER | ws::DLGFRAME) != 0 || ex_style & ex::DLGMODALFRAME != 0 {
        adjust += 1;
    }
    *r = Rect { left: r.left - adjust, top: r.top - adjust, right: r.right + adjust, bottom: r.bottom + adjust };
    if style & ws::CAPTION == ws::CAPTION {
        r.top -= if ex_style & ex::TOOLWINDOW != 0 { 17 + 1 } else { 25 + 1 };
    }
    if menu {
        r.top -= 18 + 1;
    }
    if ex_style & ex::CLIENTEDGE != 0 {
        *r = Rect { left: r.left - 2, top: r.top - 2, right: r.right + 2, bottom: r.bottom + 2 };
    }
}

/// The client area of a window with rectangle `r` (the default
/// `WM_NCCALCSIZE`): `r` minus what [`adjust_window_rect`] adds.
pub fn nc_calc_client(r: &Rect, style: u32, ex_style: u32, menu: bool) -> Rect {
    if style & ws::MINIMIZE != 0 {
        return Rect { left: r.left, top: r.top, right: r.left, bottom: r.top };
    }
    let mut empty = Rect::default();
    adjust_window_rect(&mut empty, style, menu, ex_style);
    let mut c = Rect {
        left: r.left - empty.left,
        top: r.top - empty.top,
        right: r.right - empty.right,
        bottom: r.bottom - empty.bottom,
    };
    if style & ws::VSCROLL != 0 {
        c.right -= 17;
    }
    if style & ws::HSCROLL != 0 {
        c.bottom -= 17;
    }
    if c.right < c.left {
        c.right = c.left;
    }
    if c.bottom < c.top {
        c.bottom = c.top;
    }
    c
}

// ---------------------------------------------------------------------------
// The desktop
// ---------------------------------------------------------------------------

/// Create a window structure without messages (the desktop and the message
/// parent, which Wine's explorer and wineserver provide).
fn create_bare(u: &mut User, class_proc: usize, parent: u32, rect: Rect, style: u32, tid: u32) -> Option<u32> {
    let atom = u.builtin_atoms[class_proc];
    let ci = class::find_class(u, &NameOrAtom::Atom(atom), 0)?;
    let cl = &mut u.classes[ci];
    cl.windows += 1;
    let (cid, proc_, extra) = (cl.id, cl.wndproc, cl.win_extra);
    let h = u.alloc_handle(ObjKind::Window)?;
    u.windows.insert(
        h,
        Window {
            hwnd: h,
            parent,
            owner: 0,
            children: Vec::new(),
            class: cid,
            style,
            ex_style: 0,
            id: 0,
            instance: 0,
            user_data: 0,
            wndproc: proc_,
            unicode: true,
            text: Vec::new(),
            window_rect: rect,
            client_rect: rect,
            normal_rect: rect,
            extra: vec![0; extra as usize],
            props: Vec::new(),
            tid,
            flags: 0,
            fnid: 0x8000 | class_proc as u16,
            dpi_context: 0,
            menu: 0,
            destroying: false,
            update: Rect::default(),
            erase: false,
            internal_paint: false,
            surface: 0,
            own_dc: 0,
            dlg_info: 0,
            private: Vec::new(),
            present_rect: Rect::default(),
        },
    );
    if let Some(p) = u.windows.get_mut(&parent) {
        p.children.insert(0, h);
    }
    Some(h)
}

/// The desktop window, created (with the builtin classes, and user32's
/// `InitBuiltinClasses` callback) the first time anything asks.
pub fn get_desktop_window(c: &mut Call<'_>) -> R<u32> {
    let p64 = c.g.p64();
    let tid = c.t.tid;
    let callback = {
        let mut u = c.p.user.lock().unwrap();
        if u.desktop != 0 && u.builtins_registered {
            return Ok(u.desktop);
        }
        if u.desktop == 0 {
            class::register_builtin(&mut u, p64, super::procs::DESKTOP);
            class::register_builtin(&mut u, p64, super::procs::MESSAGE);
            let m = super::sysparams::Mode::unpack_current(&c.p.display_mode);
            let screen = Rect { left: 0, top: 0, right: m.width, bottom: m.height };
            let desktop = create_bare(&mut u, super::procs::DESKTOP, 0, screen, ws::POPUP | ws::VISIBLE | ws::CLIPCHILDREN, tid)
                .ok_or(Status(crate::nt::st::NO_MEMORY))?;
            u.desktop = desktop;
            let msg = create_bare(&mut u, super::procs::MESSAGE, 0, Rect::default(), ws::POPUP | ws::CLIPCHILDREN, tid)
                .ok_or(Status(crate::nt::st::NO_MEMORY))?;
            u.message_parent = msg;
        }
        if u.builtins_registered {
            false
        } else {
            u.builtins_registered = true;
            for (proc_, ..) in class::builtin_classes(p64) {
                class::register_builtin(&mut u, p64, proc_);
            }
            true
        }
    };
    if callback {
        super::winproc::user_callback(c, super::callbacks::INIT_BUILTIN_CLASSES, &[])?;
    }
    Ok(c.p.user.lock().unwrap().desktop)
}

// ---------------------------------------------------------------------------
// Creation
// ---------------------------------------------------------------------------

const CW_USEDEFAULT: i32 = 0x8000_0000u32 as i32;

/// `NtUserCreateWindowEx(ex_style, class_name, version, window_name, style,
/// x, y, cx, cy, parent, menu, class_instance, params, flags, instance,
/// class, ansi)`.
pub fn nt_user_create_window_ex(c: &mut Call<'_>) -> R64 {
    let ex_style = c.a32(0);
    let (class_p, version_p, name_p) = (c.a(1), c.a(2), c.a(3));
    let style = c.a32(4);
    let (mut x, mut y, mut cx, mut cy) = (c.a32(5) as i32, c.a32(6) as i32, c.a32(7) as i32, c.a32(8) as i32);
    let (parent_h, menu, class_instance, params) = (c.a32(9), c.a(10), c.a(11), c.a(12));
    let (_flags, instance, class_str, ansi) = (c.a32(13), c.a(14), c.a(15), c.a32(16) != 0);
    let class_name = read_name(&c.g, class_p)?.ok_or(Status(crate::nt::st::INVALID_PARAMETER))?;
    let _version_len = if version_p != 0 { c.g.u16(version_p)? as usize / 2 } else { 0 };
    let window_name = match read_name(&c.g, name_p)? {
        Some(NameOrAtom::Name(n)) => Some(n),
        Some(NameOrAtom::Atom(a)) => Some(vec![0xffff, a]),
        None => None,
    };
    let name_buf = if name_p != 0 { c.g.ptr(name_p + off!(c.p.abi, unicode_string::BUFFER))? } else { 0 };
    if c.p.cfg.trace {
        let n = |v: &NameOrAtom| match v {
            NameOrAtom::Name(n) => crate::nt::wstr(n),
            NameOrAtom::Atom(a) => format!("#{a}"),
        };
        let title = window_name.as_ref().filter(|w| w.first() != Some(&0xffff)).map(|w| crate::nt::wstr(w));
        crate::nt_log!("[nt:{:x}]   window {:?} {:?}", c.t.tid, n(&class_name), title.unwrap_or_default());
    }
    let desktop = get_desktop_window(c)?;
    let tid = c.t.tid;

    // Parent and owner (Wine's rules).
    const HWND_MESSAGE: u32 = 0xffff_fffd;
    let mut owner = 0;
    let parent = if parent_h == HWND_MESSAGE {
        c.p.user.lock().unwrap().message_parent
    } else if parent_h != 0 {
        let full_parent = full(&c.p.user.lock().unwrap(), parent_h);
        if full_parent == 0 {
            set_last_error(c, err::INVALID_WINDOW_HANDLE);
            return Ok(0);
        }
        if style & (ws::CHILD | ws::POPUP) != ws::CHILD {
            owner = full_parent;
            desktop
        } else {
            full_parent
        }
    } else {
        if style & (ws::CHILD | ws::POPUP) == ws::CHILD {
            set_last_error(c, err::TLW_WITH_WSCHILD);
            return Ok(0);
        }
        desktop
    };

    // fix_cs_coordinates
    if style & (ws::CHILD | ws::POPUP) != 0 {
        if x == CW_USEDEFAULT {
            x = 0;
            y = 0;
        }
        if cx == CW_USEDEFAULT {
            cx = 0;
            cy = 0;
        }
    } else {
        let work = super::sysparams::primary_rect(c);
        if x == CW_USEDEFAULT {
            x = work.left;
            y = work.top;
        }
        if cx == CW_USEDEFAULT {
            cx = work.width() * 3 / 4 - x;
            cy = work.height() * 3 / 4 - y;
        } else if cy == CW_USEDEFAULT {
            cy = work.height() * 3 / 4 - y;
        }
    }
    // fix_exstyle
    let mut ex = ex_style;
    if ex & ws::ex::DLGMODALFRAME != 0 || (ex & ws::ex::STATICEDGE == 0 && style & (ws::DLGFRAME | ws::THICKFRAME) != 0) {
        ex |= ws::ex::WINDOWEDGE;
    } else {
        ex &= !ws::ex::WINDOWEDGE;
    }

    // The window structure.
    let hwnd = {
        let mut u = c.p.user.lock().unwrap();
        let Some(ci) = class::find_class(&u, &class_name, class_instance) else {
            drop(u);
            set_last_error(c, err::CLASS_DOES_NOT_EXIST);
            return Ok(0);
        };
        let cl = &mut u.classes[ci];
        cl.windows += 1;
        let (cid, wndproc, win_extra, fnid) = (cl.id, cl.wndproc, cl.win_extra, cl.fnid);
        // A window is Unicode if its winproc has a W side.
        let unicode = match class::winproc_index(&u, wndproc) {
            Some(i) => u.winprocs[i].1 != 0,
            None => !ansi,
        };
        let Some(h) = u.alloc_handle(ObjKind::Window) else {
            drop(u);
            set_last_error(c, err::NOT_ENOUGH_MEMORY);
            return Ok(0);
        };
        let mut st = style & !ws::VISIBLE;
        if st & (ws::CHILD | ws::POPUP) != ws::CHILD {
            st |= ws::CLIPSIBLINGS;
            if st & ws::POPUP == 0 {
                st |= ws::CAPTION;
            }
        }
        let mut flags = 0;
        if st & (ws::CHILD | ws::POPUP) == 0 {
            flags |= WIN_NEED_SIZE;
        }
        let r = Rect { left: x, top: y, right: x.saturating_add(cx.max(0)), bottom: y.saturating_add(cy.max(0)) };
        let child = st & (ws::CHILD | ws::POPUP) == ws::CHILD;
        u.windows.insert(
            h,
            Window {
                hwnd: h,
                parent,
                owner,
                children: Vec::new(),
                class: cid,
                style: st,
                ex_style: ex & !ws::ex::LAYERED,
                id: if child { menu } else { 0 },
                instance: if instance != 0 { instance } else { class_instance },
                user_data: 0,
                wndproc,
                unicode,
                text: Vec::new(),
                window_rect: r,
                client_rect: r,
                normal_rect: r,
                extra: vec![0; win_extra as usize],
                props: Vec::new(),
                tid,
                flags,
                fnid: if fnid & 0x8000 != 0 { 0 } else { fnid },
                dpi_context: 0,
                menu: if child { 0 } else { menu },
                destroying: false,
                update: Rect::default(),
                erase: false,
                internal_paint: false,
                surface: 0,
                own_dc: 0,
                dlg_info: 0,
                private: Vec::new(),
                present_rect: Rect::default(),
            },
        );
        if let Some(p) = u.windows.get_mut(&parent) {
            if child {
                p.children.push(h);
            } else {
                p.children.insert(0, h);
            }
        }
        h
    };

    // The CREATESTRUCTW the messages carry, and the class name it names.
    let class_for_cs: NameOrAtom = if class_str >> 16 == 0 && class_str != 0 {
        NameOrAtom::Atom(class_str as u16)
    } else {
        let u = c.p.user.lock().unwrap();
        let w = find(&u, hwnd).unwrap();
        let cl = class_of(&u, w);
        NameOrAtom::Name(cl.name[cl.name_offset.min(cl.name.len())..].to_vec())
    };
    let mut cs = winproc::CreateStruct {
        params,
        instance: if instance != 0 { instance } else { class_instance },
        menu,
        parent: if parent_h == 0 { 0 } else { parent_h as u64 },
        cx,
        cy,
        x,
        y,
        style,
        name: window_name.clone(),
        name_ptr: name_buf,
        class: class_for_cs,
        ex_style: ex,
    };

    // WM_GETMINMAXINFO, and fix the size.
    if style & ws::THICKFRAME != 0 || style & (ws::POPUP | ws::CHILD) == 0 {
        let info = get_min_max_info(c, hwnd)?;
        cx = cx.min(info.max_track.0).max(info.min_track.0);
        cy = cy.min(info.max_track.1).max(info.min_track.1);
    }
    cx = cx.max(0);
    cy = cy.max(0);
    {
        let mut u = c.p.user.lock().unwrap();
        if let Some(w) = u.windows.get_mut(&hwnd) {
            let r = Rect { left: x, top: y, right: x.saturating_add(cx), bottom: y.saturating_add(cy) };
            w.window_rect = r;
            w.client_rect = r;
        }
    }
    cs.cx = cx;
    cs.cy = cy;

    // WM_NCCREATE
    let r = winproc::send_message(c, hwnd, wm::NCCREATE, 0, 0, Pack::Create(cs.clone()))?;
    if r == 0 {
        destroy_window(c, hwnd)?;
        return Ok(0);
    }

    // WM_NCCALCSIZE (screen coordinates when wparam is FALSE).
    let (wr, parent_origin) = {
        let u = c.p.user.lock().unwrap();
        let Some(w) = u.windows.get(&hwnd) else { return Ok(0) };
        (w.window_rect, client_origin(&u, w.parent))
    };
    let screen = wr.offset(parent_origin.0, parent_origin.1);
    let (_, client) = winproc::send_nccalcsize(c, hwnd, screen)?;
    {
        let mut u = c.p.user.lock().unwrap();
        if let Some(w) = u.windows.get_mut(&hwnd) {
            w.client_rect = client.offset(-parent_origin.0, -parent_origin.1);
        }
    }

    // WM_CREATE
    let r = winproc::send_message(c, hwnd, wm::CREATE, 0, 0, Pack::Create(cs))?;
    if r as i64 == -1 || !is_window(c, hwnd) {
        destroy_window(c, hwnd)?;
        return Ok(0);
    }

    // The size messages, for windows that don't owe them to ShowWindow.
    let need_size = c.p.user.lock().unwrap().windows.get(&hwnd).is_some_and(|w| w.flags & WIN_NEED_SIZE != 0);
    if !need_size {
        send_size_move(c, hwnd)?;
    }
    if style & (ws::MINIMIZE | ws::MAXIMIZE) != 0 {
        return Err(c.unimplemented("CreateWindowEx with WS_MINIMIZE or WS_MAXIMIZE"));
    }

    // WM_PARENTNOTIFY to the parent of a child.
    if style & ws::CHILD != 0 && ex & ws::ex::NOPARENTNOTIFY == 0 {
        let id = c.p.user.lock().unwrap().windows.get(&hwnd).map(|w| w.id).unwrap_or(0);
        let wparam = (wm::CREATE as u64) | ((id & 0xffff) << 16);
        winproc::send_message(c, parent, wm::PARENTNOTIFY, wparam, handle_ret(hwnd), Pack::None)?;
        if !is_window(c, hwnd) {
            return Ok(0);
        }
    }

    if style & ws::VISIBLE != 0 {
        show_window(c, hwnd, 5)?; // SW_SHOW
    }
    Ok(handle_ret(hwnd))
}

/// WM_SIZE and WM_MOVE with the window's current client rectangle.
pub fn send_size_move(c: &mut Call<'_>, hwnd: u32) -> R<()> {
    let cr = {
        let u = c.p.user.lock().unwrap();
        match u.windows.get(&hwnd) {
            Some(w) => w.client_rect,
            None => return Ok(()),
        }
    };
    let size = (cr.width() as u32 as u64 & 0xffff) | ((cr.height() as u32 as u64 & 0xffff) << 16);
    winproc::send_message(c, hwnd, wm::SIZE, 0, size, Pack::None)?;
    let pos = (cr.left as u32 as u64 & 0xffff) | ((cr.top as u32 as u64 & 0xffff) << 16);
    winproc::send_message(c, hwnd, wm::MOVE, 0, pos, Pack::None)?;
    if let Some(w) = find_mut(&mut c.p.user.lock().unwrap(), hwnd) {
        w.flags &= !WIN_NEED_SIZE;
    }
    Ok(())
}

pub struct MinMax {
    pub reserved: (i32, i32),
    pub max_size: (i32, i32),
    pub max_pos: (i32, i32),
    pub min_track: (i32, i32),
    pub max_track: (i32, i32),
}

/// Send WM_GETMINMAXINFO with Wine's defaults (`get_min_max_info`).
pub fn get_min_max_info(c: &mut Call<'_>, hwnd: u32) -> R<MinMax> {
    let (style, ex, wr) = {
        let u = c.p.user.lock().unwrap();
        let w = u.windows.get(&hwnd).ok_or(Status(crate::nt::st::INVALID_HANDLE))?;
        (w.style, w.ex_style, w.window_rect)
    };
    let mut adj = Rect::default();
    adjust_window_rect(&mut adj, style & !(ws::VSCROLL | ws::HSCROLL), false, ex);
    let work = super::sysparams::primary_rect(c);
    let info = MinMax {
        reserved: (wr.width(), wr.height()),
        max_size: (work.width() - adj.left + adj.right, work.height() - adj.top + adj.bottom),
        max_pos: (adj.left, adj.top),
        min_track: if style & ws::CAPTION == ws::CAPTION { (119, 34) } else { (0, 0) },
        max_track: (1932, 1092),
    };
    winproc::send_getminmaxinfo(c, hwnd, info)
}

// ---------------------------------------------------------------------------
// Destruction
// ---------------------------------------------------------------------------

pub fn is_window(c: &Call<'_>, hwnd: u32) -> bool {
    full(&c.p.user.lock().unwrap(), hwnd) != 0
}

/// WM_DESTROY to the window, then to its children (Wine's
/// `send_destroy_message`).
fn send_destroy(c: &mut Call<'_>, hwnd: u32) -> R<()> {
    winproc::send_message(c, hwnd, wm::DESTROY, 0, 0, Pack::None)?;
    let children = c.p.user.lock().unwrap().windows.get(&hwnd).map(|w| w.children.clone()).unwrap_or_default();
    for ch in children {
        if is_window(c, ch) {
            send_destroy(c, ch)?;
        }
    }
    Ok(())
}

/// Children first, WM_NCDESTROY, then free (Wine's `destroy_window`).
pub fn destroy_window(c: &mut Call<'_>, hwnd: u32) -> R<()> {
    let children = c.p.user.lock().unwrap().windows.get(&hwnd).map(|w| w.children.clone()).unwrap_or_default();
    for ch in children {
        if is_window(c, ch) {
            destroy_window(c, ch)?;
        }
    }
    if is_window(c, hwnd) {
        winproc::send_message(c, hwnd, wm::NCDESTROY, 0, 0, Pack::None)?;
    }
    let (surface, own_dc) = c.p.user.lock().unwrap().windows.get(&hwnd).map(|w| (w.surface, w.own_dc)).unwrap_or((0, 0));
    if surface != 0 {
        crate::nt::display::remove(c, hwnd);
    }
    super::paint::window_destroyed(c, surface, own_dc)?;
    let mut u = c.p.user.lock().unwrap();
    if let Some(w) = u.windows.remove(&hwnd) {
        if let Some(p) = u.windows.get_mut(&w.parent) {
            p.children.retain(|&h| h != hwnd);
        }
        if let Some(cl) = u.classes.iter_mut().find(|cl| cl.id == w.class) {
            cl.windows = cl.windows.saturating_sub(1);
        }
        for t in u.threads.values_mut() {
            t.queue.forget_window(hwnd);
            if t.focus == hwnd {
                t.focus = 0;
            }
            if t.active == hwnd {
                t.active = 0;
            }
            if t.capture == hwnd {
                t.capture = 0;
            }
        }
        if u.foreground == hwnd {
            u.foreground = 0;
        }
        // Owned windows lose their owner.
        for o in u.windows.values_mut() {
            if o.owner == hwnd {
                o.owner = 0;
            }
        }
        u.free_handle(hwnd);
    }
    Ok(())
}

/// DestroyWindow for the calling thread's window: owned popups, hide, then
/// WM_DESTROY and WM_NCDESTROY down the tree.
pub fn destroy_full(c: &mut Call<'_>, hwnd: u32) -> R<bool> {
    let (desktop, tid, destroying) = {
        let u = c.p.user.lock().unwrap();
        let Some(w) = u.windows.get(&hwnd) else { return Ok(false) };
        (u.desktop, w.tid, w.destroying)
    };
    if hwnd == desktop {
        set_last_error(c, err::ACCESS_DENIED);
        return Ok(false);
    }
    if tid != c.t.tid {
        return Err(c.unimplemented("DestroyWindow on another thread's window"));
    }
    if destroying {
        return Ok(true);
    }
    find_mut(&mut c.p.user.lock().unwrap(), hwnd).unwrap().destroying = true;
    // Owned popups go first.
    let owned: Vec<u32> = {
        let u = c.p.user.lock().unwrap();
        u.windows.values().filter(|w| w.owner == hwnd).map(|w| w.hwnd).collect()
    };
    for o in owned {
        if is_window(c, o) {
            destroy_full(c, o)?;
        }
    }
    // Hide it without activating anything else.
    let visible = c.p.user.lock().unwrap().windows.get(&hwnd).is_some_and(|w| w.style & ws::VISIBLE != 0);
    if visible {
        set_window_pos(c, hwnd, 0, 0, 0, 0, 0, swp::HIDEWINDOW | swp::NOACTIVATE | swp::NOZORDER | swp::NOMOVE | swp::NOSIZE)?;
    }
    if !is_window(c, hwnd) {
        return Ok(true);
    }
    send_destroy(c, hwnd)?;
    if is_window(c, hwnd) {
        destroy_window(c, hwnd)?;
    }
    Ok(true)
}

/// `NtUserDestroyWindow(hwnd)`.
pub fn nt_user_destroy_window(c: &mut Call<'_>) -> R64 {
    let hwnd = full(&c.p.user.lock().unwrap(), c.a32(0));
    if hwnd == 0 {
        set_last_error(c, err::INVALID_WINDOW_HANDLE);
        return Ok(0);
    }
    Ok(destroy_full(c, hwnd)? as u64)
}

/// Destroy the calling thread's windows (thread exit).
pub fn destroy_thread_windows(c: &mut Call<'_>) -> R<()> {
    let tid = c.t.tid;
    let mine: Vec<u32> = {
        let u = c.p.user.lock().unwrap();
        let desktop = u.desktop;
        u.windows.values().filter(|w| w.tid == tid && w.parent == desktop).map(|w| w.hwnd).collect()
    };
    for h in mine {
        if is_window(c, h) {
            destroy_window(c, h)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Positions and visibility
// ---------------------------------------------------------------------------

/// Move a child in its parent's z order.
fn set_zorder(u: &mut User, hwnd: u32, after: u32) {
    const HWND_TOP: u32 = 0;
    const HWND_BOTTOM: u32 = 1;
    let Some(parent) = u.windows.get(&hwnd).map(|w| w.parent) else { return };
    let Some(p) = u.windows.get_mut(&parent) else { return };
    p.children.retain(|&h| h != hwnd);
    match after {
        HWND_TOP | 0xffff_ffff | 0xffff_fffe => p.children.insert(0, hwnd), // TOP, TOPMOST, NOTOPMOST
        HWND_BOTTOM => p.children.push(hwnd),
        a => match p.children.iter().position(|&h| h == a) {
            Some(i) => p.children.insert(i + 1, hwnd),
            None => p.children.insert(0, hwnd),
        },
    }
}

/// Wine's `set_window_pos` for the calling thread's windows: `WINDOWPOS`
/// through WM_WINDOWPOSCHANGING, WM_NCCALCSIZE when the size or frame
/// changes, apply, WM_WINDOWPOSCHANGED, activation.
#[allow(clippy::too_many_arguments)]
pub fn set_window_pos(c: &mut Call<'_>, hwnd: u32, after: u32, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> R<bool> {
    let (old_wr, old_cr, style, ex_style, parent, menu) = {
        let u = c.p.user.lock().unwrap();
        let Some(w) = u.windows.get(&hwnd) else { return Ok(false) };
        (w.window_rect, w.client_rect, w.style, w.ex_style, w.parent, w.menu != 0 && w.style & ws::CHILD == 0)
    };
    let mut flags = flags;
    let visible = style & ws::VISIBLE != 0;
    if visible && flags & swp::SHOWWINDOW != 0 || !visible && flags & swp::HIDEWINDOW != 0 {
        flags &= !(swp::SHOWWINDOW | swp::HIDEWINDOW);
    }
    if old_wr.left == x && old_wr.top == y {
        flags |= swp::NOMOVE;
    }
    if old_wr.width() == cx && old_wr.height() == cy {
        flags |= swp::NOSIZE;
    }
    let mut pos = winproc::WindowPos {
        hwnd,
        after,
        x: if flags & swp::NOMOVE != 0 { old_wr.left } else { x },
        y: if flags & swp::NOMOVE != 0 { old_wr.top } else { y },
        cx: if flags & swp::NOSIZE != 0 { old_wr.width() } else { cx.max(0) },
        cy: if flags & swp::NOSIZE != 0 { old_wr.height() } else { cy.max(0) },
        flags,
    };
    if flags & swp::NOSENDCHANGING == 0 {
        pos = winproc::send_windowpos(c, hwnd, wm::WINDOWPOSCHANGING, pos)?;
        flags = pos.flags;
    }
    let new_wr = if flags & (swp::NOMOVE | swp::NOSIZE) == (swp::NOMOVE | swp::NOSIZE) {
        old_wr
    } else {
        Rect { left: pos.x, top: pos.y, right: pos.x + pos.cx, bottom: pos.y + pos.cy }
    };
    let mut new_cr = old_cr.offset(new_wr.left - old_wr.left, new_wr.top - old_wr.top);
    if flags & swp::NOSIZE == 0 || flags & swp::FRAMECHANGED != 0 {
        let (_, c2) = winproc::send_nccalcsize_valid(c, hwnd, new_wr, old_wr, old_cr, &pos)?;
        new_cr = c2;
    }
    let _ = (ex_style, menu);
    {
        let mut u = c.p.user.lock().unwrap();
        if let Some(w) = u.windows.get_mut(&hwnd) {
            w.window_rect = new_wr;
            w.client_rect = new_cr;
            if flags & swp::SHOWWINDOW != 0 {
                w.style |= ws::VISIBLE;
            } else if flags & swp::HIDEWINDOW != 0 {
                w.style &= !ws::VISIBLE;
            }
        }
        if flags & swp::NOZORDER == 0 {
            set_zorder(&mut u, hwnd, pos.after);
        }
    }
    let _ = parent;
    // A shown window that moved goes to the display at its new place.
    let has_surface = c.p.user.lock().unwrap().windows.get(&hwnd).is_some_and(|w| w.surface != 0 && w.style & ws::VISIBLE != 0);
    if has_surface && flags & swp::NOMOVE == 0 {
        crate::nt::display::publish(c, hwnd);
    }
    // What became visible or changed size is repainted whole.
    let now_visible = c.p.user.lock().unwrap().windows.get(&hwnd).is_some_and(|w| w.style & ws::VISIBLE != 0);
    if now_visible
        && (flags & swp::SHOWWINDOW != 0 || (flags & swp::NOREDRAW == 0 && (new_wr.width(), new_wr.height()) != (old_wr.width(), old_wr.height())))
    {
        super::paint::window_exposed(c, hwnd);
    }
    // Activation.
    if flags & (swp::NOACTIVATE | swp::HIDEWINDOW) == 0 && style & ws::CHILD == 0 {
        set_active_window(c, hwnd, false)?;
    }
    if flags & swp::HIDEWINDOW != 0 {
        crate::nt::display::remove(c, hwnd);
        let active = c.p.user.lock().unwrap().threads.get(&c.t.tid).map(|t| t.active).unwrap_or(0);
        if active == hwnd {
            set_active_window(c, 0, false)?;
        }
    }
    pos.x = new_wr.left;
    pos.y = new_wr.top;
    pos.cx = new_wr.width();
    pos.cy = new_wr.height();
    pos.flags = flags;
    if flags & (swp::NOMOVE | swp::NOSIZE | swp::SHOWWINDOW | swp::HIDEWINDOW | swp::FRAMECHANGED) != (swp::NOMOVE | swp::NOSIZE)
        || flags & swp::NOZORDER == 0
    {
        winproc::send_windowpos(c, hwnd, wm::WINDOWPOSCHANGED, pos)?;
    }
    Ok(true)
}

/// `NtUserSetWindowPos(hwnd, after, x, y, cx, cy, flags)`.
pub fn nt_user_set_window_pos(c: &mut Call<'_>) -> R64 {
    let (h, after, x, y, cx, cy, flags) =
        (c.a32(0), c.a32(1), c.a32(2) as i32, c.a32(3) as i32, c.a32(4) as i32, c.a32(5) as i32, c.a32(6));
    let hwnd = full(&c.p.user.lock().unwrap(), h);
    if hwnd == 0 {
        set_last_error(c, err::INVALID_WINDOW_HANDLE);
        return Ok(0);
    }
    if c.p.user.lock().unwrap().windows[&hwnd].tid != c.t.tid {
        return Err(c.unimplemented("SetWindowPos on another thread's window"));
    }
    if flags & swp::ASYNCWINDOWPOS != 0 {
        return Err(c.unimplemented("SetWindowPos with SWP_ASYNCWINDOWPOS"));
    }
    Ok(set_window_pos(c, hwnd, after, x, y, cx, cy, flags)? as u64)
}

/// `NtUserMoveWindow(hwnd, x, y, cx, cy, repaint)`.
pub fn nt_user_move_window(c: &mut Call<'_>) -> R64 {
    let (h, x, y, cx, cy, repaint) = (c.a32(0), c.a32(1) as i32, c.a32(2) as i32, c.a32(3) as i32, c.a32(4) as i32, c.a32(5));
    let hwnd = full(&c.p.user.lock().unwrap(), h);
    if hwnd == 0 {
        set_last_error(c, err::INVALID_WINDOW_HANDLE);
        return Ok(0);
    }
    let mut flags = swp::NOZORDER | swp::NOACTIVATE;
    if repaint == 0 {
        flags |= swp::NOREDRAW;
    }
    Ok(set_window_pos(c, hwnd, 0, x, y, cx, cy, flags)? as u64)
}

/// Wine's `show_window` (SW_* commands without minimize/maximize).
pub fn show_window(c: &mut Call<'_>, hwnd: u32, cmd: u32) -> R<bool> {
    const SW_HIDE: u32 = 0;
    const SW_SHOWNORMAL: u32 = 1;
    const SW_SHOWNOACTIVATE: u32 = 4;
    const SW_SHOW: u32 = 5;
    const SW_SHOWNA: u32 = 8;
    const SW_RESTORE: u32 = 9;
    const SW_SHOWDEFAULT: u32 = 10;
    let (style, was_visible) = {
        let u = c.p.user.lock().unwrap();
        let Some(w) = u.windows.get(&hwnd) else { return Ok(false) };
        (w.style, w.style & ws::VISIBLE != 0)
    };
    let mut swp_flags = swp::NOSIZE | swp::NOMOVE;
    let show = match cmd {
        SW_HIDE => {
            if !was_visible {
                return Ok(false);
            }
            swp_flags |= swp::HIDEWINDOW | swp::NOACTIVATE | swp::NOZORDER;
            false
        }
        SW_SHOWNOACTIVATE | SW_SHOWNA => {
            swp_flags |= swp::SHOWWINDOW | swp::NOACTIVATE | swp::NOZORDER;
            true
        }
        SW_SHOW | SW_SHOWNORMAL | SW_RESTORE | SW_SHOWDEFAULT => {
            if style & ws::CHILD != 0 {
                swp_flags |= swp::NOACTIVATE | swp::NOZORDER;
            }
            swp_flags |= swp::SHOWWINDOW;
            true
        }
        _ => return Err(c.unimplemented(&format!("ShowWindow command {cmd} (minimize/maximize)"))),
    };
    if show != was_visible {
        winproc::send_message(c, hwnd, wm::SHOWWINDOW, show as u64, 0, Pack::None)?;
        if !is_window(c, hwnd) {
            return Ok(was_visible);
        }
    }
    set_window_pos(c, hwnd, 0, 0, 0, 0, 0, swp_flags)?;
    let need_size = c.p.user.lock().unwrap().windows.get(&hwnd).is_some_and(|w| w.flags & WIN_NEED_SIZE != 0);
    if need_size && show {
        send_size_move(c, hwnd)?;
    }
    Ok(was_visible)
}

/// `NtUserShowWindow(hwnd, cmd)`.
pub fn nt_user_show_window(c: &mut Call<'_>) -> R64 {
    let (h, cmd) = (c.a32(0), c.a32(1));
    let hwnd = full(&c.p.user.lock().unwrap(), h);
    if hwnd == 0 {
        set_last_error(c, err::INVALID_WINDOW_HANDLE);
        return Ok(0);
    }
    if c.p.user.lock().unwrap().windows[&hwnd].tid != c.t.tid {
        return Err(c.unimplemented("ShowWindow on another thread's window"));
    }
    Ok(show_window(c, hwnd, cmd)? as u64)
}

// ---------------------------------------------------------------------------
// Activation and focus
// ---------------------------------------------------------------------------

/// Make `hwnd` (a top-level window, or 0) the active window of its thread,
/// with the messages Windows sends: WM_NCACTIVATE/WM_ACTIVATE to the old and
/// the new, WM_ACTIVATEAPP when the foreground thread changes, then focus.
pub fn set_active_window(c: &mut Call<'_>, hwnd: u32, _mouse: bool) -> R<u32> {
    let tid = c.t.tid;
    let prev = {
        let mut u = c.p.user.lock().unwrap();
        let t = u.thread(tid);
        if t.active == hwnd {
            return Ok(hwnd);
        }
        std::mem::replace(&mut t.active, hwnd)
    };
    let was_foreground = c.p.user.lock().unwrap().foreground;
    if prev != 0 && is_window(c, prev) {
        winproc::send_message(c, prev, wm::NCACTIVATE, 0, 0, Pack::None)?;
        winproc::send_message(c, prev, wm::ACTIVATE, 0, handle_ret(hwnd), Pack::None)?;
    }
    if hwnd == 0 {
        c.p.user.lock().unwrap().foreground = 0;
        return Ok(prev);
    }
    if was_foreground == 0 {
        winproc::send_message(c, hwnd, wm::ACTIVATEAPP, 1, 0, Pack::None)?;
    }
    c.p.user.lock().unwrap().foreground = hwnd;
    winproc::send_message(c, hwnd, wm::NCACTIVATE, 1, 0, Pack::None)?;
    winproc::send_message(c, hwnd, wm::ACTIVATE, 1, handle_ret(prev), Pack::None)?;
    // Focus follows activation unless the window proc moved it elsewhere.
    let focus = c.p.user.lock().unwrap().thread(tid).focus;
    if focus == 0 || !is_child_or_self(&c.p.user.lock().unwrap(), hwnd, focus) {
        set_focus(c, hwnd)?;
    }
    Ok(prev)
}

fn is_child_or_self(u: &User, parent: u32, mut h: u32) -> bool {
    while h != 0 {
        if h == parent {
            return true;
        }
        h = match u.windows.get(&h) {
            Some(w) if w.style & ws::CHILD != 0 => w.parent,
            _ => 0,
        };
    }
    false
}

/// Move the keyboard focus: WM_KILLFOCUS to the old, WM_SETFOCUS to the new.
pub fn set_focus(c: &mut Call<'_>, hwnd: u32) -> R<u32> {
    let tid = c.t.tid;
    let prev = std::mem::replace(&mut c.p.user.lock().unwrap().thread(tid).focus, hwnd);
    if prev == hwnd {
        return Ok(prev);
    }
    if prev != 0 && is_window(c, prev) {
        winproc::send_message(c, prev, wm::KILLFOCUS, handle_ret(hwnd), 0, Pack::None)?;
    }
    if hwnd != 0 && is_window(c, hwnd) {
        winproc::send_message(c, hwnd, wm::SETFOCUS, handle_ret(prev), 0, Pack::None)?;
    }
    Ok(prev)
}

pub fn nt_user_set_focus(c: &mut Call<'_>) -> R64 {
    let h = c.a32(0);
    let hwnd = if h == 0 { 0 } else { full(&c.p.user.lock().unwrap(), h) };
    if h != 0 && hwnd == 0 {
        set_last_error(c, err::INVALID_WINDOW_HANDLE);
        return Ok(0);
    }
    if hwnd != 0 {
        // Activate the top-level window first.
        let top = {
            let u = c.p.user.lock().unwrap();
            let mut t = hwnd;
            while let Some(w) = u.windows.get(&t) {
                if w.style & ws::CHILD == 0 {
                    break;
                }
                t = w.parent;
            }
            t
        };
        let active = c.p.user.lock().unwrap().thread(c.t.tid).active;
        if active != top {
            set_active_window(c, top, false)?;
        }
    }
    Ok(handle_ret(set_focus(c, hwnd)?))
}

pub fn nt_user_set_active_window(c: &mut Call<'_>) -> R64 {
    let h = c.a32(0);
    let hwnd = if h == 0 { 0 } else { full(&c.p.user.lock().unwrap(), h) };
    Ok(handle_ret(set_active_window(c, hwnd, false)?))
}

pub fn nt_user_get_foreground_window(c: &mut Call<'_>) -> R64 {
    Ok(handle_ret(c.p.user.lock().unwrap().foreground))
}

pub fn nt_user_set_foreground_window(c: &mut Call<'_>) -> R64 {
    let hwnd = full(&c.p.user.lock().unwrap(), c.a32(0));
    if hwnd == 0 {
        return Ok(0);
    }
    set_active_window(c, hwnd, false)?;
    Ok(1)
}

/// `NtUserEnableWindow(hwnd, enable)`: the previous disabled state.
pub fn nt_user_enable_window(c: &mut Call<'_>) -> R64 {
    let (h, enable) = (c.a32(0), c.a32(1) != 0);
    let hwnd = full(&c.p.user.lock().unwrap(), h);
    if hwnd == 0 {
        set_last_error(c, err::INVALID_WINDOW_HANDLE);
        return Ok(0);
    }
    let was_disabled = {
        let mut u = c.p.user.lock().unwrap();
        let w = find_mut(&mut u, hwnd).unwrap();
        let was = w.style & ws::DISABLED != 0;
        if enable {
            w.style &= !ws::DISABLED;
        } else {
            w.style |= ws::DISABLED;
        }
        was
    };
    if was_disabled == enable {
        winproc::send_message(c, hwnd, wm::ENABLE, enable as u64, 0, Pack::None)?;
    }
    Ok(was_disabled as u64)
}

/// `NtUserGetWindowPlacement(hwnd, *placement)`.
pub fn nt_user_get_window_placement(c: &mut Call<'_>) -> R64 {
    let (h, p) = (c.a32(0), c.a(1));
    let u = c.p.user.lock().unwrap();
    let Some(w) = find(&u, h) else {
        drop(u);
        set_last_error(c, err::INVALID_WINDOW_HANDLE);
        return Ok(0);
    };
    let (show, normal) = (if w.style & ws::VISIBLE != 0 { 1 } else { 0 }, w.normal_rect);
    drop(u);
    // length, flags, showCmd, ptMinPosition, ptMaxPosition, rcNormalPosition
    c.g.w32(p + 4, 0)?;
    c.g.w32(p + 8, if show == 0 { 1 } else { 1 })?;
    for o in [12u64, 16, 20, 24] {
        c.g.w32(p + o, u32::MAX)?;
    }
    write_rect(&mut c.g, p + 28, &normal)?;
    Ok(1)
}

// ---------------------------------------------------------------------------
// Window data
// ---------------------------------------------------------------------------

pub mod gwl {
    pub const WNDPROC: i32 = -4;
    pub const HINSTANCE: i32 = -6;
    pub const HWNDPARENT: i32 = -8;
    pub const STYLE: i32 = -16;
    pub const EXSTYLE: i32 = -20;
    pub const USERDATA: i32 = -21;
    pub const ID: i32 = -12;
}

/// `GetWindowLong*` (through `NtUserCallHwndParam`).
pub fn get_window_long(c: &mut Call<'_>, h: u32, offset: i32, size: u32, ansi: bool) -> R64 {
    let u = c.p.user.lock().unwrap();
    let Some(w) = find(&u, h) else {
        drop(u);
        set_last_error(c, err::INVALID_WINDOW_HANDLE);
        return Ok(0);
    };
    let v = match offset {
        gwl::WNDPROC => class::get_winproc(&u, w.wndproc, ansi),
        gwl::HINSTANCE => w.instance,
        gwl::HWNDPARENT => {
            if w.style & ws::CHILD != 0 {
                handle_ret(w.parent)
            } else {
                handle_ret(w.owner)
            }
        }
        gwl::STYLE => w.style as u64,
        gwl::EXSTYLE => w.ex_style as u64,
        gwl::USERDATA => w.user_data,
        gwl::ID => w.id,
        o if o >= 0 && o as usize + size as usize <= w.extra.len() => {
            let mut b = [0u8; 8];
            b[..size as usize].copy_from_slice(&w.extra[o as usize..o as usize + size as usize]);
            u64::from_le_bytes(b)
        }
        _ => {
            drop(u);
            set_last_error(c, err::INVALID_INDEX);
            return Ok(0);
        }
    };
    // A LONG is sign-extended when returned as a LONG_PTR.
    Ok(match size {
        4 => v as u32 as i32 as i64 as u64,
        2 => v & 0xffff,
        _ => v,
    })
}

/// `NtUserSetWindowLong(Ptr)`/`NtUserSetWindowWord(hwnd, offset, value[, ansi])`.
pub fn nt_user_set_window_long(c: &mut Call<'_>, size: u32) -> R64 {
    let (h, offset, value) = (c.a32(0), c.a32(1) as i32, c.a(2));
    let ansi = size != 2 && c.a32(3) != 0;
    let value = if size == 4 { value as u32 as i32 as i64 as u64 } else { value };
    let hwnd = full(&c.p.user.lock().unwrap(), h);
    if hwnd == 0 {
        set_last_error(c, err::INVALID_WINDOW_HANDLE);
        return Ok(0);
    }
    let old = match offset {
        gwl::STYLE | gwl::EXSTYLE => {
            // WM_STYLECHANGING / WM_STYLECHANGED with STYLESTRUCT {old, new}.
            let old = {
                let u = c.p.user.lock().unwrap();
                let w = &u.windows[&hwnd];
                if offset == gwl::STYLE { w.style } else { w.ex_style }
            };
            let new = winproc::send_stylechange(c, hwnd, wm::STYLECHANGING, offset, old, value as u32)?;
            {
                let mut u = c.p.user.lock().unwrap();
                let w = u.windows.get_mut(&hwnd).unwrap();
                if offset == gwl::STYLE {
                    w.style = new;
                } else {
                    w.ex_style = new;
                }
            }
            winproc::send_stylechange(c, hwnd, wm::STYLECHANGED, offset, old, new)?;
            old as u64
        }
        _ => {
            let mut u = c.p.user.lock().unwrap();
            let new_proc = if offset == gwl::WNDPROC { class::alloc_winproc_locked(&mut u, value, ansi) } else { 0 };
            let new_unicode = match class::winproc_index(&u, new_proc) {
                Some(i) => u.winprocs[i].1 != 0,
                None => !ansi,
            };
            let old_proc = u.windows.get(&hwnd).map(|w| w.wndproc).unwrap_or(0);
            let old_proc_ptr = class::get_winproc(&u, old_proc, ansi);
            let w = u.windows.get_mut(&hwnd).unwrap();
            match offset {
                gwl::WNDPROC => {
                    w.wndproc = new_proc;
                    w.unicode = new_unicode;
                    old_proc_ptr
                }
                gwl::HINSTANCE => std::mem::replace(&mut w.instance, value),
                gwl::USERDATA => std::mem::replace(&mut w.user_data, value),
                gwl::ID => std::mem::replace(&mut w.id, value),
                gwl::HWNDPARENT => {
                    drop(u);
                    return Err(c.unimplemented("SetWindowLong(GWLP_HWNDPARENT)"));
                }
                o if o >= 0 && o as usize + size as usize <= w.extra.len() => {
                    let r = o as usize..o as usize + size as usize;
                    let mut b = [0u8; 8];
                    b[..size as usize].copy_from_slice(&w.extra[r.clone()]);
                    w.extra[r].copy_from_slice(&value.to_le_bytes()[..size as usize]);
                    u64::from_le_bytes(b)
                }
                _ => {
                    drop(u);
                    set_last_error(c, err::INVALID_INDEX);
                    return Ok(0);
                }
            }
        }
    };
    Ok(match size {
        4 => old as u32 as i32 as i64 as u64,
        2 => old & 0xffff,
        _ => old,
    })
}

/// Properties: a string names an atom (added on set), or is an integer atom.
fn prop_key(c: &Call<'_>, p: u64, add: bool) -> R<Option<(u16, bool)>> {
    if p >> 16 == 0 {
        return Ok(Some((p as u16, false)));
    }
    let name = super::read_wstrz(&c.g, p)?;
    let mut u = c.p.user.lock().unwrap();
    Ok(if add { Some((u.add_atom(&name), true)) } else { u.find_atom(&name).map(|a| (a, true)) })
}

/// `NtUserGetProp(hwnd, str)`.
pub fn nt_user_get_prop(c: &mut Call<'_>) -> R64 {
    let (h, s) = (c.a32(0), c.a(1));
    let Some((atom, _)) = prop_key(c, s, false)? else { return Ok(0) };
    let u = c.p.user.lock().unwrap();
    Ok(find(&u, h).and_then(|w| w.props.iter().find(|p| p.atom == atom)).map(|p| p.data).unwrap_or(0))
}

/// `NtUserSetProp(hwnd, str, handle)`.
pub fn nt_user_set_prop(c: &mut Call<'_>) -> R64 {
    let (h, s, data) = (c.a32(0), c.a(1), c.a(2));
    let Some((atom, string)) = prop_key(c, s, true)? else { return Ok(0) };
    let mut u = c.p.user.lock().unwrap();
    let Some(w) = find_mut(&mut u, h) else {
        drop(u);
        set_last_error(c, err::INVALID_WINDOW_HANDLE);
        return Ok(0);
    };
    match w.props.iter_mut().find(|p| p.atom == atom) {
        Some(p) => p.data = data,
        None => w.props.push(Prop { atom, string, data }),
    }
    Ok(1)
}

/// `NtUserRemoveProp(hwnd, str)`.
pub fn nt_user_remove_prop(c: &mut Call<'_>) -> R64 {
    let (h, s) = (c.a32(0), c.a(1));
    let Some((atom, _)) = prop_key(c, s, false)? else { return Ok(0) };
    let mut u = c.p.user.lock().unwrap();
    let Some(w) = find_mut(&mut u, h) else { return Ok(0) };
    match w.props.iter().position(|p| p.atom == atom) {
        Some(i) => Ok(w.props.remove(i).data),
        None => Ok(0),
    }
}

// ---------------------------------------------------------------------------
// The tree
// ---------------------------------------------------------------------------

/// `NtUserBuildHwndList(desktop, hwnd, children, non_immersive, thread,
/// count, *buffer, *size)`: the children of `hwnd` (the desktop's for 0) in
/// z order, then a terminating 1 (`HWND_BOTTOM`), as Wine's list has.
pub fn nt_user_build_hwnd_list(c: &mut Call<'_>) -> R64 {
    let (_desk, h, children, _ni, tid, count, buf, size_p) =
        (c.a32(0), c.a32(1), c.a32(2) != 0, c.a32(3), c.a32(4), c.a32(5), c.a(6), c.a(7));
    let mut list = Vec::new();
    {
        let u = c.p.user.lock().unwrap();
        let root = if h == 0 { u.desktop } else { full(&u, h) };
        fn walk(u: &User, h: u32, recurse: bool, tid: u32, out: &mut Vec<u32>) {
            if let Some(w) = u.windows.get(&h) {
                for &ch in &w.children {
                    if tid == 0 || u.windows.get(&ch).is_some_and(|x| x.tid == tid) {
                        out.push(ch);
                    }
                    if recurse {
                        walk(u, ch, true, tid, out);
                    }
                }
            }
        }
        walk(&u, root, children, tid, &mut list);
    }
    list.push(1);
    c.g.w32(size_p, list.len() as u32)?;
    if (list.len() as u32) > count {
        return Ok(crate::nt::st::BUFFER_TOO_SMALL as u64);
    }
    let ps = c.g.psize();
    for (i, &hw) in list.iter().enumerate() {
        c.g.wptr(buf + i as u64 * ps, handle_ret(hw))?;
    }
    Ok(0)
}

/// `NtUserGetAncestor(hwnd, type)`: GA_PARENT, GA_ROOT, GA_ROOTOWNER.
pub fn nt_user_get_ancestor(c: &mut Call<'_>) -> R64 {
    let (h, ty) = (c.a32(0), c.a32(1));
    let u = c.p.user.lock().unwrap();
    let Some(w) = find(&u, h) else { return Ok(0) };
    let desktop = u.desktop;
    let r = match ty {
        1 => w.parent, // GA_PARENT
        2 | 3 => {
            // GA_ROOT: walk parents to the child of the desktop;
            // GA_ROOTOWNER: then owners.
            let mut cur = w.hwnd;
            while let Some(x) = u.windows.get(&cur) {
                if x.parent == desktop || x.parent == 0 || x.parent == u.message_parent {
                    break;
                }
                cur = x.parent;
            }
            if ty == 3 {
                while let Some(x) = u.windows.get(&cur) {
                    if x.owner == 0 {
                        break;
                    }
                    cur = x.owner;
                }
            }
            cur
        }
        _ => 0,
    };
    Ok(handle_ret(r))
}

/// `NtUserSetParent(hwnd, parent)`.
pub fn nt_user_set_parent(c: &mut Call<'_>) -> R64 {
    Err(c.unimplemented("NtUserSetParent"))
}

/// `NtUserFindWindowEx(parent, child, class, title, unk)`.
pub fn nt_user_find_window_ex(c: &mut Call<'_>) -> R64 {
    let (parent, after, class_p, title_p) = (c.a32(0), c.a32(1), c.a(2), c.a(3));
    let class_name = read_name(&c.g, class_p)?;
    let title = match read_name(&c.g, title_p)? {
        Some(NameOrAtom::Name(n)) => Some(n),
        _ => None,
    };
    let u = c.p.user.lock().unwrap();
    let root = if parent == 0 { u.desktop } else { full(&u, parent) };
    let atom = match &class_name {
        Some(NameOrAtom::Atom(a)) => Some(*a),
        Some(NameOrAtom::Name(n)) => match u.find_atom(n) {
            Some(a) => Some(a),
            None => return Ok(0),
        },
        None => None,
    };
    let Some(r) = u.windows.get(&root) else { return Ok(0) };
    let mut started = after == 0;
    for &ch in &r.children {
        if !started {
            started = full(&u, after) == ch;
            continue;
        }
        let w = &u.windows[&ch];
        if let Some(a) = atom {
            if class_of(&u, w).atom != a {
                continue;
            }
        }
        if let Some(t) = &title {
            if &w.text != t {
                continue;
            }
        }
        return Ok(handle_ret(ch));
    }
    Ok(0)
}

/// `NtUserInternalGetWindowText(hwnd, text, count)`.
pub fn nt_user_internal_get_window_text(c: &mut Call<'_>) -> R64 {
    let (h, p, count) = (c.a32(0), c.a(1), c.a32(2) as usize);
    let text = {
        let u = c.p.user.lock().unwrap();
        match find(&u, h) {
            Some(w) => w.text.clone(),
            None => {
                drop(u);
                set_last_error(c, err::INVALID_WINDOW_HANDLE);
                return Ok(0);
            }
        }
    };
    if count == 0 {
        return Ok(0);
    }
    let n = text.len().min(count - 1);
    let mut z = text[..n].to_vec();
    z.push(0);
    c.g.write_wchars(p, &z)?;
    Ok(n as u64)
}

/// `NtUserQueryWindow(hwnd, class)`.
pub fn nt_user_query_window(c: &mut Call<'_>) -> R64 {
    let (h, class) = (c.a32(0), c.a32(1));
    let u = c.p.user.lock().unwrap();
    let Some(w) = find(&u, h) else { return Ok(0) };
    let tid = w.tid;
    match class {
        0 | 1 => Ok(c.p.pid as u64),  // WindowProcess
        2 => Ok(tid as u64),         // WindowThread
        3 => Ok(handle_ret(u.threads.get(&tid).map(|t| t.active).unwrap_or(0))),
        4 => Ok(handle_ret(u.threads.get(&tid).map(|t| t.focus).unwrap_or(0))),
        5 => Ok(0),                  // WindowIsHung
        8 | 9 => Ok(0),              // default IME window / input context: none
        _ => {
            drop(u);
            Err(c.unimplemented(&format!("NtUserQueryWindow class {class}")))
        }
    }
}

/// `NtUserSetWindowFNID(hwnd, fnid)`.
pub fn nt_user_set_window_fnid(c: &mut Call<'_>) -> R64 {
    let (h, fnid) = (c.a32(0), c.a32(1) as u16);
    let mut u = c.p.user.lock().unwrap();
    let Some(w) = find_mut(&mut u, h) else { return Ok(0) };
    w.fnid = fnid;
    Ok(1)
}

/// `NtUserCallHwnd(hwnd, code)`.
pub fn call_hwnd(c: &mut Call<'_>) -> R64 {
    let (h, code) = (c.a32(0), c.a32(1));
    let u = c.p.user.lock().unwrap();
    let w = find(&u, h);
    let r = match code {
        // GetDialogInfo
        1 => w.map(|w| w.dlg_info).unwrap_or(0),
        // GetDpiForWindow
        2 => w.map(|_| super::sysparams::DPI as u64).unwrap_or(0),
        // GetParent: the parent of a child, the owner of a popup.
        5 => w
            .map(|w| {
                if w.style & ws::CHILD != 0 {
                    handle_ret(w.parent)
                } else if w.style & ws::POPUP != 0 {
                    handle_ret(w.owner)
                } else {
                    0
                }
            })
            .unwrap_or(0),
        // GetWindowDpiAwarenessContext
        6 => w.map(|_| u.process_dpi_context as u64).unwrap_or(0),
        // GetWindowInputContext: no IME contexts.
        7 => 0,
        // GetWindowTextLength
        9 => w.map(|w| w.text.len() as u64).unwrap_or(0),
        // IsWindow
        10 => w.is_some() as u64,
        // IsWindowEnabled
        11 => w.is_some_and(|w| w.style & ws::DISABLED == 0) as u64,
        // IsWindowUnicode
        12 => w.is_some_and(|w| w.unicode) as u64,
        // IsWindowVisible: it and every parent up to the desktop.
        13 => {
            let mut vis = w.is_some();
            let mut cur = w.map(|w| w.hwnd).unwrap_or(0);
            while vis {
                match u.windows.get(&cur) {
                    Some(x) if cur == u.desktop => {
                        let _ = x;
                        break;
                    }
                    Some(x) => {
                        vis = x.style & ws::VISIBLE != 0;
                        cur = x.parent;
                    }
                    None => break,
                }
            }
            vis as u64
        }
        // NtUserGetFullWindowHandle
        15 => w.map(|w| handle_ret(w.hwnd)).unwrap_or(0),
        // NtUserIsCurrentProcessWindow
        16 => w.map(|w| handle_ret(w.hwnd)).unwrap_or(0),
        // NtUserIsCurrentThreadWindow
        17 => w.filter(|w| w.tid == c.t.tid).map(|w| handle_ret(w.hwnd)).unwrap_or(0),
        _ => {
            drop(u);
            return Err(c.unimplemented(&format!("NtUserCallHwnd code {code}")));
        }
    };
    Ok(r)
}

/// `NtUserCallHwndParam(hwnd, param, code)`.
pub fn call_hwnd_param(c: &mut Call<'_>) -> R64 {
    let (h, param, code) = (c.a32(0), c.a(1), c.a32(2));
    let ps = c.p.abi.ptr_size() as u32;
    match code {
        // ClientToScreen / ScreenToClient: (POINT *)
        0 | 23 => {
            let (x, y) = {
                let u = c.p.user.lock().unwrap();
                match find(&u, h) {
                    Some(w) => client_origin(&u, w.hwnd),
                    None => {
                        drop(u);
                        set_last_error(c, err::INVALID_WINDOW_HANDLE);
                        return Ok(0);
                    }
                }
            };
            let (dx, dy) = if code == 0 { (x, y) } else { (-x, -y) };
            let px = c.g.u32(param)? as i32 + dx;
            let py = c.g.u32(param + 4)? as i32 + dy;
            c.g.w32(param, px as u32)?;
            c.g.w32(param + 4, py as u32)?;
            Ok(1)
        }
        // GetClassLongA/W, GetClassLongPtrA/W, GetClassWord
        2 => class::get_class_long(c, h, param as i32, 4, true),
        3 => class::get_class_long(c, h, param as i32, 4, false),
        4 => class::get_class_long(c, h, param as i32, ps, true),
        5 => class::get_class_long(c, h, param as i32, ps, false),
        6 => class::get_class_long(c, h, param as i32, 2, true),
        // GetWindowLongA/W, GetWindowLongPtrA/W, GetWindowWord
        9 => get_window_long(c, h, param as i32, 4, true),
        10 => get_window_long(c, h, param as i32, 4, false),
        11 => get_window_long(c, h, param as i32, ps, true),
        12 => get_window_long(c, h, param as i32, ps, false),
        18 => get_window_long(c, h, param as i32, 2, true),
        // GetWindowRect / GetClientRect: (get_window_rects_params *)
        13 | 14 => {
            let rect_p = c.g.ptr(param)?;
            let r = {
                let u = c.p.user.lock().unwrap();
                match find(&u, h) {
                    Some(w) if code == 13 => screen_window_rect(&u, w),
                    Some(w) => Rect { left: 0, top: 0, right: w.client_rect.width(), bottom: w.client_rect.height() },
                    None => {
                        drop(u);
                        set_last_error(c, err::INVALID_WINDOW_HANDLE);
                        return Ok(0);
                    }
                }
            };
            write_rect(&mut c.g, rect_p, &r)?;
            Ok(1)
        }
        // GetWindowRelative (GetWindow): GW_HWNDFIRST, LAST, NEXT, PREV,
        // OWNER, CHILD.
        16 => {
            let u = c.p.user.lock().unwrap();
            let Some(w) = find(&u, h) else { return Ok(0) };
            let siblings = u.windows.get(&w.parent).map(|p| p.children.clone()).unwrap_or_default();
            let pos = siblings.iter().position(|&x| x == w.hwnd);
            let r = match param {
                0 => siblings.first().copied().unwrap_or(0),
                1 => siblings.last().copied().unwrap_or(0),
                2 => pos.and_then(|i| siblings.get(i + 1)).copied().unwrap_or(0),
                3 => pos.filter(|&i| i > 0).map(|i| siblings[i - 1]).unwrap_or(0),
                4 => w.owner,
                5 => w.children.first().copied().unwrap_or(0),
                _ => 0,
            };
            Ok(handle_ret(r))
        }
        // GetWindowThread: (DWORD *process)
        17 => {
            let u = c.p.user.lock().unwrap();
            let Some(tid) = find(&u, h).map(|w| w.tid) else { return Ok(0) };
            drop(u);
            c.g.out32(param, c.p.pid)?;
            Ok(tid as u64)
        }
        // IsChild(parent, child)
        19 => {
            let u = c.p.user.lock().unwrap();
            let parent = full(&u, h);
            let mut cur = full(&u, param as u32);
            while let Some(w) = u.windows.get(&cur) {
                if w.style & ws::CHILD == 0 {
                    break;
                }
                if w.parent == parent {
                    return Ok(1);
                }
                cur = w.parent;
            }
            Ok(0)
        }
        // MapWindowPoints: (map_window_points_params *) {hwnd_to, points,
        // count, dpi}; returns MAKELONG(dx, dy).
        20 => {
            let ps64 = c.g.psize();
            let to = c.g.ptr(param)? as u32;
            let pts = c.g.ptr(param + ps64)?;
            let count = c.g.u32(param + 2 * ps64)?;
            let (fx, fy, tx, ty) = {
                let u = c.p.user.lock().unwrap();
                let from = if h == 0 { (0, 0) } else { client_origin(&u, full(&u, h)) };
                let tov = if to == 0 { (0, 0) } else { client_origin(&u, full(&u, to)) };
                (from.0, from.1, tov.0, tov.1)
            };
            let (dx, dy) = (fx - tx, fy - ty);
            for i in 0..count as u64 {
                let x = c.g.u32(pts + 8 * i)? as i32 + dx;
                let y = c.g.u32(pts + 8 * i + 4)? as i32 + dy;
                c.g.w32(pts + 8 * i, x as u32)?;
                c.g.w32(pts + 8 * i + 4, y as u32)?;
            }
            Ok((dx as u32 as u64 & 0xffff) | ((dy as u32 as u64 & 0xffff) << 16))
        }
        // MonitorFromWindow
        22 => Ok(handle_ret(super::sysparams::MONITOR)),
        // SetDialogInfo
        24 => {
            let mut u = c.p.user.lock().unwrap();
            let Some(w) = find_mut(&mut u, h) else { return Ok(0) };
            w.dlg_info = param;
            Ok(1)
        }
        // GetPrivateData / SetPrivateData: ({offset, size[, value]} *); the
        // value there before.
        30 | 31 => {
            let (offset, size) = (c.g.u32(param)? as usize, c.g.u32(param + 4)? as usize);
            let value = if code == 31 { c.g.ptr(param + 8)? } else { 0 };
            // A builtin control's state is a few words.
            if size == 0 || size > 8 || offset > 256 {
                return Ok(0);
            }
            let mut u = c.p.user.lock().unwrap();
            let Some(w) = find_mut(&mut u, h) else {
                drop(u);
                set_last_error(c, err::INVALID_WINDOW_HANDLE);
                return Ok(0);
            };
            if w.private.len() < offset + size {
                w.private.resize(offset + size, 0);
            }
            let mut old = [0u8; 8];
            old[..size].copy_from_slice(&w.private[offset..offset + size]);
            if code == 31 {
                w.private[offset..offset + size].copy_from_slice(&value.to_le_bytes()[..size]);
            }
            Ok(u64::from_le_bytes(old))
        }
        _ => Err(c.unimplemented(&format!("NtUserCallHwndParam code {code}"))),
    }
}

#[allow(dead_code)]
fn _unused(g: &crate::nt::G<'_>) -> R<Rect> {
    read_rect(g, 0)
}
