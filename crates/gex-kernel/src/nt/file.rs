//! Files over gex's VFS (docs/nt-layer.md §3.2).
//!
//! NT paths name DOS drives through `\??\X:\`: `C:` is the staged prefix's
//! `drive_c`, `Z:` is the guest's `/`. Lookups are case-insensitive, as
//! Windows' are: an exact match first, then a scan of each directory.

use super::object::Obj;
use super::{st, to_w, wstr, Abi, Call, Status, R};
use crate::fs::oflags;
use crate::vfs::{FileHandle, FileType, Metadata};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// An open NT file.
#[derive(Debug)]
pub struct NtFile {
    pub handle: Arc<dyn FileHandle>,
    /// Guest (Unix-style) path; empty for the console streams.
    pub path: String,
    /// The NT path it was opened by, `\??\C:\…`.
    pub nt_path: Vec<u16>,
    pub pos: AtomicU64,
    pub options: u32,
    pub access: u32,
    pub is_dir: bool,
    pub kind: FileKind,
    pub delete_on_close: AtomicBool,
    /// Directory enumeration state for `NtQueryDirectoryFile`.
    pub dir: Mutex<Option<DirScan>>,
    /// A change notification armed on this directory (FindFirstChange-
    /// Notification): the sync id of the manual-reset state that waits on
    /// the handle see, and whether the subtree counts.
    pub notify: Mutex<Option<(u64, bool)>>,
    /// Where a rename moved the file (`path` is where it was opened).
    pub moved_to: Mutex<Option<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Disk,
    /// A standard stream: the gex process's fd 0/1/2.
    Console,
    /// `\Device\Null` / `NUL`.
    Null,
    /// One end of a named pipe (`pipe.rs`).
    Pipe,
}

#[derive(Debug)]
pub struct DirScan {
    pub entries: Vec<(String, Metadata)>,
    pub next: usize,
    pub mask: Option<Vec<u16>>,
}

pub mod opt {
    pub const DIRECTORY_FILE: u32 = 0x1;
    pub const SYNCHRONOUS_IO_ALERT: u32 = 0x10;
    pub const SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
    pub const NON_DIRECTORY_FILE: u32 = 0x40;
    pub const DELETE_ON_CLOSE: u32 = 0x1000;
    pub const OPEN_REPARSE_POINT: u32 = 0x200000;
}

pub mod disp {
    pub const SUPERSEDE: u32 = 0;
    pub const OPEN: u32 = 1;
    pub const CREATE: u32 = 2;
    pub const OPEN_IF: u32 = 3;
    pub const OVERWRITE: u32 = 4;
    pub const OVERWRITE_IF: u32 = 5;
}

pub mod attr {
    pub const READONLY: u32 = 0x1;
    pub const HIDDEN: u32 = 0x2;
    pub const DIRECTORY: u32 = 0x10;
    pub const ARCHIVE: u32 = 0x20;
    pub const NORMAL: u32 = 0x80;
}


const GENERIC_WRITE: u32 = 0x4000_0000;
const GENERIC_ALL: u32 = 0x1000_0000;

const FILE_WRITE_DATA: u32 = 0x2;
const FILE_APPEND_DATA: u32 = 0x4;


/// Where an NT path leads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A guest VFS path.
    Path(String),
    Null,
    /// A device or namespace gex does not provide.
    Unsupported(String),
    /// `\??\pipe\<name>`: the name, lower-cased (`pipe\…`).
    Pipe(String),
}

/// The guest path of drive `letter`'s root, if it exists.
fn drive_root(c: &Call<'_>, letter: u8) -> Option<String> {
    match letter.to_ascii_uppercase() {
        b'C' => Some(format!("{}/drive_c", c.p.cfg.prefix)),
        b'Z' => Some(String::new()),
        _ => None,
    }
}

/// Translate an NT path (`\??\C:\x`, `\DosDevices\…`, `\Device\Null`).
pub fn nt_to_target(c: &Call<'_>, nt: &[u16]) -> R<Target> {
    let s = wstr(nt);
    let lower = s.to_ascii_lowercase();
    let rest = if let Some(r) = ["\\??\\", "\\dosdevices\\", "\\global??\\"]
        .iter()
        .find_map(|p| lower.starts_with(p).then(|| &s[p.len()..]))
    {
        r.to_string()
    } else if lower == "\\device\\null" {
        return Ok(Target::Null);
    } else {
        return Ok(Target::Unsupported(s));
    };
    let rl = rest.to_ascii_lowercase();
    if rl == "nul" {
        return Ok(Target::Null);
    }
    // The named pipe namespace: only this process's own pipes exist (there
    // are no services and no wineserver), so any other pipe is absent, which
    // is what a client of a stopped service sees.
    if let Some(name) = super::pipe::pipe_name(&rl) {
        return Ok(Target::Pipe(name));
    }
    let b = rest.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        let root = drive_root(c, b[0]).ok_or(Status(st::OBJECT_PATH_NOT_FOUND))?;
        let tail = rest[2..].replace('\\', "/");
        // A 32-bit process sees syswow64 as its system32: gex presents a
        // 32-bit Windows to it (docs/nt-layer.md §8).
        let tail = if c.p.abi == super::Abi::P32 && b[0].eq_ignore_ascii_case(&b'C') {
            let l = tail.to_ascii_lowercase();
            match l.strip_prefix("/windows/system32") {
                Some(r) if r.is_empty() || r.starts_with('/') => format!("/windows/syswow64{}", &tail["/windows/system32".len()..]),
                _ => tail,
            }
        } else {
            tail
        };
        let tail = tail.trim_start_matches('/');
        return Ok(Target::Path(if tail.is_empty() {
            if root.is_empty() { "/".into() } else { root }
        } else {
            format!("{root}/{tail}")
        }));
    }
    Ok(Target::Unsupported(s))
}

/// Resolve `path` case-insensitively. `Ok(Err(parent_exists))` when the last
/// component is missing (so a create can proceed if the parent exists).
fn resolve_ci(c: &Call<'_>, path: &str) -> Result<String, bool> {
    let vfs = c.kernel().vfs();
    if vfs.metadata(path).is_ok() {
        return Ok(path.to_string());
    }
    let comps: Vec<&str> = path.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    let mut cur = String::new();
    for (i, comp) in comps.iter().enumerate() {
        let last = i + 1 == comps.len();
        if *comp == ".." {
            cur = crate::vfs::parent_of(if cur.is_empty() { "/" } else { &cur });
            if cur == "/" {
                cur.clear();
            }
            continue;
        }
        let exact = format!("{cur}/{comp}");
        if vfs.metadata(&exact).is_ok() {
            cur = exact;
            continue;
        }
        let dir = if cur.is_empty() { "/".to_string() } else { cur.clone() };
        let found = vfs
            .open(&dir, oflags::O_RDONLY | oflags::O_DIRECTORY, 0)
            .and_then(|h| h.read_dir())
            .ok()
            .and_then(|es| es.into_iter().find(|e| e.name.eq_ignore_ascii_case(comp)));
        match found {
            Some(e) => cur = format!("{cur}/{}", e.name),
            None => return Err(last && vfs.metadata(&dir).is_ok()),
        }
    }
    Ok(if cur.is_empty() { "/".into() } else { cur })
}

/// The full NT name for an `OBJECT_ATTRIBUTES`, resolving a root directory
/// handle.
pub fn full_nt_name(c: &Call<'_>, root: u32, name: &[u16]) -> R<Vec<u16>> {
    if root == 0 {
        return Ok(name.to_vec());
    }
    let st = c.p.st.lock().unwrap();
    let e = st.handles.get(root).ok_or(Status(st::INVALID_HANDLE))?;
    match &e.obj {
        Obj::File(f) if f.is_dir => {
            let mut v = f.nt_path.clone();
            if v.last() != Some(&(b'\\' as u16)) {
                v.push(b'\\' as u16);
            }
            v.extend_from_slice(name);
            Ok(v)
        }
        Obj::File(_) => Err(Status(st::NOT_A_DIRECTORY)),
        _ => Err(Status(st::OBJECT_TYPE_MISMATCH)),
    }
}

/// Unix seconds → FILETIME.
pub fn filetime(unix_secs: i64) -> u64 {
    ((unix_secs + 11_644_473_600).max(0) as u64) * 10_000_000
}

pub fn file_attributes(md: &Metadata, name: &str) -> u32 {
    let mut a = if md.file_type == FileType::Directory { attr::DIRECTORY } else { attr::ARCHIVE };
    if md.mode & 0o222 == 0 {
        a |= attr::READONLY;
    }
    if name.starts_with('.') && name != "." && name != ".." {
        a |= attr::HIDDEN;
    }
    a
}

fn wants_write(access: u32) -> bool {
    access & (GENERIC_WRITE | GENERIC_ALL | FILE_WRITE_DATA | FILE_APPEND_DATA) != 0
}

/// Open or create. Returns the file and the `IO_STATUS_BLOCK` information.
/// What the NT layer did with one guest path (`set_file_hook`): a session
/// recorder (gex-eject) learns from it which files a Windows program needs.
#[derive(Debug, Clone)]
pub struct FileAccess<'a> {
    /// The guest (Unix-style) path, resolved case-insensitively when it
    /// exists.
    pub path: &'a str,
    pub read: bool,
    pub write: bool,
    pub create: bool,
    pub directory: bool,
    /// Whether the open or query succeeded.
    pub ok: bool,
}

type FileHook = Box<dyn Fn(&FileAccess<'_>) + Send + Sync>;
static FILE_HOOK: std::sync::OnceLock<FileHook> = std::sync::OnceLock::new();

/// Have `f` told about every path the NT layer opens or queries (`NtCreateFile`,
/// `NtOpenFile`, the attribute queries, and files it reads itself such as NLS
/// tables). Once per host process; none by default.
pub fn set_file_hook(f: impl Fn(&FileAccess<'_>) + Send + Sync + 'static) {
    let _ = FILE_HOOK.set(Box::new(f));
}

fn note_access(c: &Call<'_>, nt: &[u16], write: bool, create: bool, directory: bool, ok: bool) {
    let Some(hook) = FILE_HOOK.get() else { return };
    let Ok(Target::Path(p)) = nt_to_target(c, nt) else { return };
    let path = resolve_ci(c, &p).unwrap_or(p);
    hook(&FileAccess { path: &path, read: !create, write, create, directory, ok });
}

fn open(c: &mut Call<'_>, nt: &[u16], access: u32, disposition: u32, options: u32) -> R<(Arc<NtFile>, u64)> {
    let r = open_inner(c, nt, access, disposition, options);
    if FILE_HOOK.get().is_some() {
        let creates = matches!(disposition, disp::CREATE | disp::OPEN_IF | disp::OVERWRITE_IF | disp::SUPERSEDE);
        match &r {
            Ok((f, info)) => note_access(c, nt, wants_write(access), *info == 2, f.is_dir, true),
            Err(_) => note_access(c, nt, wants_write(access), creates, options & opt::DIRECTORY_FILE != 0, false),
        }
    }
    if c.p.cfg.trace {
        let s = match &r {
            Ok(_) => "ok".to_string(),
            Err(e) => format!("{:#010x}", e.0),
        };
        crate::nt_log!("[nt:{:x}]   {} {:?} -> {s}", c.t.tid, c.name(), wstr(nt));
    }
    r
}

fn open_inner(c: &mut Call<'_>, nt: &[u16], access: u32, disposition: u32, options: u32) -> R<(Arc<NtFile>, u64)> {
    let target = nt_to_target(c, nt)?;
    let path = match target {
        Target::Null => {
            let f = NtFile {
                handle: Arc::new(crate::vfs::PathHandle(Metadata::regular(0))),
                path: String::new(),
                nt_path: nt.to_vec(),
                pos: AtomicU64::new(0),
                options,
                access,
                is_dir: false,
                kind: FileKind::Null,
                delete_on_close: AtomicBool::new(false),
                dir: Mutex::new(None),
                notify: Mutex::new(None),
                moved_to: Mutex::new(None),
            };
            return Ok((Arc::new(f), 1));
        }
        Target::Unsupported(s) => {
            // Named pipes, mailslots, devices and the console driver: not
            // provided yet. Loud, because a program may depend on them.
            return Err(c.unimplemented(&format!("{} of the NT path {s:?}", c.name())));
        }
        Target::Pipe(name) => {
            let handle = super::pipe::open_client(&c.p.pipes, &name)?;
            return Ok((Arc::new(pipe_file(handle, nt, options, access)), 1));
        }
        Target::Path(p) => p,
    };
    let (resolved, exists) = match resolve_ci(c, &path) {
        Ok(p) => (p, true),
        Err(true) => (path.clone(), false),
        Err(false) => return Err(Status(st::OBJECT_PATH_NOT_FOUND)),
    };
    let md = if exists { c.kernel().vfs().metadata(&resolved).ok() } else { None };
    let is_dir = md.as_ref().is_some_and(|m| m.file_type == FileType::Directory);
    if is_dir && options & opt::NON_DIRECTORY_FILE != 0 {
        return Err(Status(st::FILE_IS_A_DIRECTORY));
    }
    if exists && !is_dir && options & opt::DIRECTORY_FILE != 0 {
        return Err(Status(st::NOT_A_DIRECTORY));
    }
    let write = wants_write(access);
    let mut flags = if write { oflags::O_RDWR } else { oflags::O_RDONLY };
    let info;
    match disposition {
        disp::OPEN => {
            if !exists {
                return Err(Status(st::OBJECT_NAME_NOT_FOUND));
            }
            info = 1; // FILE_OPENED
        }
        disp::CREATE => {
            if exists {
                return Err(Status(st::OBJECT_NAME_COLLISION));
            }
            flags |= oflags::O_CREAT | oflags::O_EXCL;
            info = 2; // FILE_CREATED
        }
        disp::OPEN_IF => {
            if !exists {
                flags |= oflags::O_CREAT;
            }
            info = if exists { 1 } else { 2 };
        }
        disp::OVERWRITE | disp::OVERWRITE_IF | disp::SUPERSEDE => {
            if !exists && disposition == disp::OVERWRITE {
                return Err(Status(st::OBJECT_NAME_NOT_FOUND));
            }
            if is_dir {
                return Err(Status(st::FILE_IS_A_DIRECTORY));
            }
            flags |= oflags::O_CREAT | oflags::O_TRUNC;
            if !write {
                flags = (flags & !oflags::O_RDONLY) | oflags::O_RDWR;
            }
            info = if !exists { 2 } else if disposition == disp::SUPERSEDE { 0 } else { 3 };
        }
        _ => return Err(Status(st::INVALID_PARAMETER)),
    }
    let creating_dir = !exists && options & opt::DIRECTORY_FILE != 0;
    let creating = !exists && (creating_dir || flags & oflags::O_CREAT != 0);
    let handle = {
        let vfs = c.kernel().vfs();
        if creating_dir {
            vfs.mkdir(&resolved, 0o755)?;
            vfs.open(&resolved, oflags::O_RDONLY | oflags::O_DIRECTORY, 0)?
        } else if is_dir {
            vfs.open(&resolved, oflags::O_RDONLY | oflags::O_DIRECTORY, 0)?
        } else {
            vfs.open(&resolved, flags, 0o644)?
        }
    };
    let f = NtFile {
        handle,
        path: resolved,
        nt_path: nt.to_vec(),
        pos: AtomicU64::new(0),
        options,
        access,
        is_dir: is_dir || creating_dir,
        kind: FileKind::Disk,
        delete_on_close: AtomicBool::new(options & opt::DELETE_ON_CLOSE != 0),
        dir: Mutex::new(None),
        notify: Mutex::new(None),
        moved_to: Mutex::new(None),
    };
    if creating || info == 0 || info == 3 {
        changed_path(c, &f.path);
    }
    Ok((Arc::new(f), info))
}

fn pipe_file(handle: Arc<dyn FileHandle>, nt: &[u16], options: u32, access: u32) -> NtFile {
    NtFile {
        handle,
        path: String::new(),
        nt_path: nt.to_vec(),
        pos: AtomicU64::new(0),
        options,
        access,
        is_dir: false,
        kind: FileKind::Pipe,
        delete_on_close: AtomicBool::new(false),
        dir: Mutex::new(None),
        notify: Mutex::new(None),
        moved_to: Mutex::new(None),
    }
}

/// `NtCreateNamedPipeFile(*handle, access, attr, iosb, sharing, disposition,
/// options, type, read_mode, completion_mode, max_instances, in_quota,
/// out_quota, timeout)`: the server end of a pipe in this process.
pub fn nt_create_named_pipe_file(c: &mut Call<'_>) -> R {
    let (hp, access, attr_p, iosb, options) = (c.a(0), c.a32(1), c.a(2), c.a(3), c.a32(6));
    let a = c.g.obj_attr(attr_p)?.ok_or(Status(st::INVALID_PARAMETER))?;
    let nt = full_nt_name(c, a.root, &a.name)?;
    let name = match nt_to_target(c, &nt)? {
        Target::Pipe(n) => n,
        _ => return Err(Status(st::OBJECT_NAME_INVALID)),
    };
    let r = super::pipe::create_server(&c.p.pipes, name);
    if c.p.cfg.trace {
        crate::nt_log!("[nt:{:x}]   NtCreateNamedPipeFile {:?} -> {}", c.t.tid, wstr(&nt), if r.is_ok() { "ok" } else { "in use" });
    }
    let handle = r?;
    let f = pipe_file(handle, &nt, options, access);
    let h = c.p.st.lock().unwrap().handles.insert(Obj::File(Arc::new(f)), access);
    c.g.wptr(hp, h as u64)?;
    c.g.iosb(iosb, st::SUCCESS, 2)?; // FILE_CREATED
    Ok(st::SUCCESS)
}

/// `NtCreateFile(*handle, access, attr, iosb, alloc_size, attributes, sharing,
/// disposition, options, ea, ea_len)`.
pub fn nt_create_file(c: &mut Call<'_>) -> R {
    let (hp, access, attr_p, iosb) = (c.a(0), c.a32(1), c.a(2), c.a(3));
    let (disposition, options, ea, ea_len) = (c.a32(7), c.a32(8), c.a(9), c.a32(10));
    if ea != 0 && ea_len != 0 {
        return Err(c.unimplemented("NtCreateFile with extended attributes"));
    }
    let a = c.g.obj_attr(attr_p)?.ok_or(Status(st::INVALID_PARAMETER))?;
    let nt = full_nt_name(c, a.root, &a.name)?;
    let (f, info) = open(c, &nt, access, disposition, options)?;
    let h = c.p.st.lock().unwrap().handles.insert(Obj::File(f), access);
    c.g.wptr(hp, h as u64)?;
    c.g.iosb(iosb, st::SUCCESS, info)?;
    Ok(st::SUCCESS)
}

/// `NtOpenFile(*handle, access, attr, iosb, sharing, options)`.
pub fn nt_open_file(c: &mut Call<'_>) -> R {
    let (hp, access, attr_p, iosb, options) = (c.a(0), c.a32(1), c.a(2), c.a(3), c.a32(5));
    let a = c.g.obj_attr(attr_p)?.ok_or(Status(st::INVALID_PARAMETER))?;
    let nt = full_nt_name(c, a.root, &a.name)?;
    let (f, info) = open(c, &nt, access, disp::OPEN, options)?;
    let h = c.p.st.lock().unwrap().handles.insert(Obj::File(f), access);
    c.g.wptr(hp, h as u64)?;
    c.g.iosb(iosb, st::SUCCESS, info)?;
    Ok(st::SUCCESS)
}

/// The file behind a handle.
pub fn get_file(c: &Call<'_>, h: u32) -> R<Arc<NtFile>> {
    let st = c.p.st.lock().unwrap();
    match st.handles.get(h).map(|e| &e.obj) {
        Some(Obj::File(f)) => Ok(Arc::clone(f)),
        Some(_) => Err(Status(st::OBJECT_TYPE_MISMATCH)),
        None => Err(Status(st::INVALID_HANDLE)),
    }
}

/// An I/O request's explicit offset: `None` for "the file pointer".
fn io_offset(c: &Call<'_>, p: u64) -> R<Option<u64>> {
    if p == 0 {
        return Ok(None);
    }
    let v = c.g.u64(p)?;
    // FILE_USE_FILE_POINTER_POSITION (-2) and FILE_WRITE_TO_END_OF_FILE (-1).
    if v as i64 == -2 {
        return Ok(None);
    }
    Ok(Some(v))
}

/// Finish an I/O request: the status block, the event, the APC.
fn complete_io(c: &mut Call<'_>, event: u32, apc: u64, iosb: u64, status: u32, info: u64) -> R<()> {
    c.g.iosb(iosb, status, info)?;
    if event != 0 {
        super::sync::set_event_handle(c, event)?;
    }
    if apc != 0 {
        return Err(c.unimplemented(&format!("{} with an APC completion routine", c.name())));
    }
    Ok(())
}

/// `NtReadFile(handle, event, apc, apc_ctx, iosb, buffer, len, offset, key)`.
pub fn nt_read_file(c: &mut Call<'_>) -> R {
    let (h, event, apc, iosb, buf, len, offp) =
        (c.a32(0), c.a32(1), c.a(2), c.a(4), c.a(5), c.a32(6) as usize, c.a(7));
    let f = get_file(c, h)?;
    if f.is_dir {
        return Err(Status(st::INVALID_DEVICE_REQUEST));
    }
    let offset = io_offset(c, offp)?;
    if f.options & (opt::SYNCHRONOUS_IO_ALERT | opt::SYNCHRONOUS_IO_NONALERT) == 0 && offset.is_none() && f.kind == FileKind::Disk {
        // Asynchronous I/O without an offset is invalid on Windows too.
        return Err(Status(st::INVALID_PARAMETER));
    }
    let n = match f.kind {
        FileKind::Null => 0,
        FileKind::Pipe => {
            if f.options & (opt::SYNCHRONOUS_IO_ALERT | opt::SYNCHRONOUS_IO_NONALERT) == 0 {
                return Err(c.unimplemented("NtReadFile on an overlapped pipe"));
            }
            let mut tmp = vec![0u8; len];
            let n = super::pipe::read_blocking(c, &f.handle, &mut tmp)?;
            if n == 0 && len > 0 {
                complete_io(c, event, apc, iosb, st::PIPE_BROKEN, 0)?;
                return Err(Status(st::PIPE_BROKEN));
            }
            c.g.m.write(buf, &tmp[..n]).map_err(|_| Status(st::ACCESS_VIOLATION))?;
            n
        }
        FileKind::Console => {
            let slice = c.g.m.read_mut(buf, len).map_err(|_| Status(st::ACCESS_VIOLATION))?;
            f.handle.read_at(0, slice)? as usize
        }
        FileKind::Disk => {
            let at = offset.unwrap_or_else(|| f.pos.load(Ordering::Relaxed));
            let slice = c.g.m.read_mut(buf, len).map_err(|_| Status(st::ACCESS_VIOLATION))?;
            let n = if f.handle.is_stream() { f.handle.read_at(0, slice)? } else { f.handle.read_at(at, slice)? } as usize;
            f.pos.store(at + n as u64, Ordering::Relaxed);
            n
        }
    };
    if n == 0 && len > 0 {
        complete_io(c, event, apc, iosb, st::END_OF_FILE, 0)?;
        return Err(Status(st::END_OF_FILE));
    }
    complete_io(c, event, apc, iosb, st::SUCCESS, n as u64)?;
    Ok(st::SUCCESS)
}

/// `NtWriteFile(handle, event, apc, apc_ctx, iosb, buffer, len, offset, key)`.
pub fn nt_write_file(c: &mut Call<'_>) -> R {
    let (h, event, apc, iosb, buf, len, offp) =
        (c.a32(0), c.a32(1), c.a(2), c.a(4), c.a(5), c.a32(6) as usize, c.a(7));
    let f = get_file(c, h)?;
    if f.is_dir {
        return Err(Status(st::INVALID_DEVICE_REQUEST));
    }
    let offset = io_offset(c, offp)?;
    let data = c.g.bytes(buf, len)?;
    let n = match f.kind {
        FileKind::Null => len,
        FileKind::Pipe => {
            if f.options & (opt::SYNCHRONOUS_IO_ALERT | opt::SYNCHRONOUS_IO_NONALERT) == 0 {
                return Err(c.unimplemented("NtWriteFile on an overlapped pipe"));
            }
            super::pipe::write_blocking(c, &f.handle, &data)?
        }
        FileKind::Console => write_all_stream(&f.handle, &data)?,
        FileKind::Disk => {
            let at = match offset {
                Some(v) if v as i64 == -1 => f.handle.metadata()?.size,
                Some(v) => v,
                None => f.pos.load(Ordering::Relaxed),
            };
            let n = if f.handle.is_stream() {
                write_all_stream(&f.handle, &data)?
            } else {
                f.handle.write_at(at, &data)? as usize
            };
            f.pos.store(at + n as u64, Ordering::Relaxed);
            n
        }
    };
    if f.kind == FileKind::Disk {
        changed_path(c, &f.path);
    }
    complete_io(c, event, apc, iosb, st::SUCCESS, n as u64)?;
    Ok(st::SUCCESS)
}

fn write_all_stream(h: &Arc<dyn FileHandle>, data: &[u8]) -> R<usize> {
    let mut done = 0;
    while done < data.len() {
        match h.write_at(0, &data[done..]) {
            Ok(0) => std::thread::yield_now(),
            Ok(n) => done += n as usize,
            Err(crate::Errno::EAGAIN) => std::thread::sleep(std::time::Duration::from_millis(1)),
            Err(e) if done == 0 => return Err(e.into()),
            Err(_) => break,
        }
    }
    Ok(done)
}

/// Write `FILE_BASIC_INFORMATION` (40 bytes, both ABIs).
fn write_basic(c: &mut Call<'_>, at: u64, md: &Metadata, name: &str) -> R<()> {
    let t = filetime(md.mtime);
    for i in 0..4 {
        c.g.w64(at + 8 * i, t)?;
    }
    c.g.w32(at + 32, file_attributes(md, name))?;
    c.g.w32(at + 36, 0)
}

/// Write `FILE_STANDARD_INFORMATION` (24 bytes).
fn write_standard(c: &mut Call<'_>, at: u64, md: &Metadata, delete_pending: bool) -> R<()> {
    let is_dir = md.file_type == FileType::Directory;
    let size = if is_dir { 0 } else { md.size };
    c.g.w64(at, (size + 4095) & !4095)?;
    c.g.w64(at + 8, size)?;
    c.g.w32(at + 16, md.nlink.max(1))?;
    c.g.w8(at + 20, delete_pending as u8)?;
    c.g.w8(at + 21, is_dir as u8)?;
    c.g.w16(at + 22, 0)
}

/// Write `FILE_NETWORK_OPEN_INFORMATION` (56 bytes).
fn write_network_open(c: &mut Call<'_>, at: u64, md: &Metadata, name: &str) -> R<()> {
    let t = filetime(md.mtime);
    for i in 0..4 {
        c.g.w64(at + 8 * i, t)?;
    }
    let size = if md.file_type == FileType::Directory { 0 } else { md.size };
    c.g.w64(at + 32, (size + 4095) & !4095)?;
    c.g.w64(at + 40, size)?;
    c.g.w32(at + 48, file_attributes(md, name))?;
    c.g.w32(at + 52, 0)
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The DOS-relative name Windows reports in `FileNameInformation`:
/// the path without the drive, `\windows\system32\x.dll`.
fn name_without_drive(f: &NtFile) -> Vec<u16> {
    let s = wstr(&f.nt_path);
    let s = s.strip_prefix("\\??\\").unwrap_or(&s);
    let s = if s.len() >= 2 && s.as_bytes()[1] == b':' { &s[2..] } else { s };
    to_w(s)
}

fn console_md() -> Metadata {
    let mut m = Metadata::regular(0);
    m.file_type = FileType::CharDevice;
    m
}

/// `NtQueryInformationFile(handle, iosb, buffer, len, class)`.
pub fn nt_query_information_file(c: &mut Call<'_>) -> R {
    let (h, iosb, buf, len, class) = (c.a32(0), c.a(1), c.a(2), c.a32(3) as u64, c.a32(4));
    let f = get_file(c, h)?;
    let md = match f.kind {
        FileKind::Disk => f.handle.metadata()?,
        _ => console_md(),
    };
    let name = basename(&f.path).to_string();
    let need = |n: u64| if len < n { Err(Status(st::INFO_LENGTH_MISMATCH)) } else { Ok(()) };
    let info = match class {
        // FileBasicInformation
        4 => {
            need(40)?;
            write_basic(c, buf, &md, &name)?;
            40
        }
        // FileStandardInformation
        5 => {
            need(24)?;
            write_standard(c, buf, &md, f.delete_on_close.load(Ordering::Relaxed))?;
            24
        }
        // FileInternalInformation
        6 => {
            need(8)?;
            c.g.w64(buf, md.ino)?;
            8
        }
        // FileEaInformation
        7 => {
            need(4)?;
            c.g.w32(buf, 0)?;
            4
        }
        // FileAccessInformation
        8 => {
            need(4)?;
            c.g.w32(buf, f.access)?;
            4
        }
        // FileNameInformation
        9 => {
            need(4)?;
            let n = name_without_drive(&f);
            c.g.w32(buf, n.len() as u32 * 2)?;
            let room = ((len - 4) / 2) as usize;
            c.g.write_wchars(buf + 4, &n[..n.len().min(room)])?;
            if n.len() > room {
                c.g.iosb(iosb, st::BUFFER_OVERFLOW, len)?;
                return Ok(st::BUFFER_OVERFLOW);
            }
            4 + n.len() as u64 * 2
        }
        // FilePositionInformation
        14 => {
            need(8)?;
            c.g.w64(buf, f.pos.load(Ordering::Relaxed))?;
            8
        }
        // FileModeInformation
        16 => {
            need(4)?;
            c.g.w32(buf, f.options & 0x103e)?;
            4
        }
        // FileAlignmentInformation
        17 => {
            need(4)?;
            c.g.w32(buf, 0)?;
            4
        }
        // FileAllInformation: basic, standard, internal, ea, access,
        // position, mode, alignment, name.
        18 => {
            need(100)?;
            write_basic(c, buf, &md, &name)?;
            write_standard(c, buf + 40, &md, f.delete_on_close.load(Ordering::Relaxed))?;
            c.g.w64(buf + 64, md.ino)?;
            c.g.w32(buf + 72, 0)?;
            c.g.w32(buf + 76, f.access)?;
            c.g.w64(buf + 80, f.pos.load(Ordering::Relaxed))?;
            c.g.w32(buf + 88, f.options & 0x103e)?;
            c.g.w32(buf + 92, 0)?;
            let n = name_without_drive(&f);
            c.g.w32(buf + 96, n.len() as u32 * 2)?;
            let room = ((len - 100) / 2) as usize;
            c.g.write_wchars(buf + 100, &n[..n.len().min(room)])?;
            if n.len() > room {
                c.g.iosb(iosb, st::BUFFER_OVERFLOW, len)?;
                return Ok(st::BUFFER_OVERFLOW);
            }
            100 + n.len() as u64 * 2
        }
        // FileNetworkOpenInformation
        34 => {
            need(56)?;
            write_network_open(c, buf, &md, &name)?;
            56
        }
        // FileAttributeTagInformation
        35 => {
            need(8)?;
            c.g.w32(buf, file_attributes(&md, &name))?;
            c.g.w32(buf + 4, 0)?;
            8
        }
        _ => return Err(c.unimplemented(&format!("NtQueryInformationFile class {class}"))),
    };
    c.g.iosb(iosb, st::SUCCESS, info)?;
    Ok(st::SUCCESS)
}

/// `NtSetInformationFile(handle, iosb, buffer, len, class)`.
pub fn nt_set_information_file(c: &mut Call<'_>) -> R {
    let (h, iosb, buf, len, class) = (c.a32(0), c.a(1), c.a(2), c.a32(3) as u64, c.a32(4));
    let f = get_file(c, h)?;
    match class {
        // FilePositionInformation
        14 => {
            if len < 8 {
                return Err(Status(st::INFO_LENGTH_MISMATCH));
            }
            f.pos.store(c.g.u64(buf)?, Ordering::Relaxed);
        }
        // FileDispositionInformation
        13 => {
            if len < 1 {
                return Err(Status(st::INFO_LENGTH_MISMATCH));
            }
            f.delete_on_close.store(c.g.u8(buf)? != 0, Ordering::Relaxed);
        }
        // FileEndOfFileInformation
        20 => {
            if len < 8 {
                return Err(Status(st::INFO_LENGTH_MISMATCH));
            }
            f.handle.truncate(c.g.u64(buf)?)?;
            changed_path(c, &f.path);
        }
        // FileAllocationInformation: advisory; the size does not change.
        19 => {}
        // FileRenameInformation { ReplaceIfExists, RootDirectory,
        // FileNameLength, FileName[] }
        10 => {
            let ps = c.p.abi.ptr_size();
            let replace = c.g.u8(buf)? != 0;
            let root = c.g.ptr(buf + ps)? as u32;
            let n = c.g.u32(buf + 2 * ps)? as usize;
            let name = c.g.wchars(buf + 2 * ps + 4, n / 2)?;
            let nt = full_nt_name(c, root, &name)?;
            let to = match nt_to_target(c, &nt)? {
                Target::Path(p) => p,
                _ => return Err(Status(st::OBJECT_PATH_NOT_FOUND)),
            };
            let from = f.moved_to.lock().unwrap().clone().unwrap_or_else(|| f.path.clone());
            let dest = match resolve_ci(c, &to) {
                Ok(existing) => {
                    if !replace {
                        return Err(Status(st::OBJECT_NAME_COLLISION));
                    }
                    existing
                }
                Err(true) => {
                    // The parent exists; keep the new leaf name as given.
                    let (parent, leaf) = to.rsplit_once('/').unwrap_or(("", &to));
                    let parent = resolve_ci(c, if parent.is_empty() { "/" } else { parent }).map_err(|_| Status(st::OBJECT_PATH_NOT_FOUND))?;
                    format!("{}/{leaf}", parent.trim_end_matches('/'))
                }
                Err(false) => return Err(Status(st::OBJECT_PATH_NOT_FOUND)),
            };
            c.kernel().vfs().rename(&from, &dest)?;
            *f.moved_to.lock().unwrap() = Some(dest.clone());
            changed_path(c, &from);
            changed_path(c, &dest);
        }
        _ => return Err(c.unimplemented(&format!("NtSetInformationFile class {class}"))),
    }
    c.g.iosb(iosb, st::SUCCESS, 0)?;
    Ok(st::SUCCESS)
}

/// Signal the change notifications armed on directories that contain
/// `path` (its parent, or any ancestor for a subtree watch). Only changes
/// this process makes through these calls are seen: gex has no host file
/// change source.
pub fn changed_path(c: &mut Call<'_>, path: &str) {
    let watches: Vec<(u64, String, bool)> = {
        let stl = c.p.st.lock().unwrap();
        stl.handles
            .entries()
            .filter_map(|o| match o {
                Obj::File(f) => f.notify.lock().unwrap().map(|(id, sub)| (id, f.path.clone(), sub)),
                _ => None,
            })
            .collect()
    };
    let parent = path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
    for (id, dir, subtree) in watches {
        let dir = dir.trim_end_matches('/');
        let hit = parent == dir || (subtree && path.starts_with(&format!("{dir}/")));
        if hit {
            super::sync::set_state(c, id, true);
        }
    }
}

/// `NtNotifyChangeDirectoryFile(dir, event, apc, ctx, iosb, buffer, len,
/// filter, subtree)`: the form FindFirst/NextChangeNotification use (no
/// event, APC or buffer), which arms the directory handle itself; it becomes
/// signalled on the next change ([`changed_path`]).
pub fn nt_notify_change_directory_file(c: &mut Call<'_>) -> R {
    let (h, event, apc, buf, subtree) = (c.a32(0), c.a32(1), c.a(2), c.a(5), c.a32(8) & 0xff != 0);
    let f = get_file(c, h)?;
    if !f.is_dir {
        return Err(Status(st::INVALID_PARAMETER));
    }
    if event != 0 || apc != 0 || buf != 0 {
        return Err(c.unimplemented("NtNotifyChangeDirectoryFile with an event, APC or buffer (ReadDirectoryChangesW)"));
    }
    let mut n = f.notify.lock().unwrap();
    match *n {
        // Re-armed (FindNextChangeNotification): unsignalled until the next
        // change.
        Some((id, _)) => {
            *n = Some((id, subtree));
            drop(n);
            super::sync::set_state(c, id, false);
        }
        None => {
            let id = c.p.new_sync_id();
            c.p.sync.lock().unwrap().states.insert(id, super::sync::SState::Event { set: false, manual: true });
            *n = Some((id, subtree));
        }
    }
    Ok(st::PENDING)
}

/// `NtQueryAttributesFile(attr, FILE_BASIC_INFORMATION *)`.
pub fn nt_query_attributes_file(c: &mut Call<'_>) -> R {
    let (attr_p, out) = (c.a(0), c.a(1));
    let (path, md) = stat_attr(c, attr_p)?;
    write_basic(c, out, &md, basename(&path))?;
    Ok(st::SUCCESS)
}

/// `NtQueryFullAttributesFile(attr, FILE_NETWORK_OPEN_INFORMATION *)`.
pub fn nt_query_full_attributes_file(c: &mut Call<'_>) -> R {
    let (attr_p, out) = (c.a(0), c.a(1));
    let (path, md) = stat_attr(c, attr_p)?;
    write_network_open(c, out, &md, basename(&path))?;
    Ok(st::SUCCESS)
}

fn stat_attr(c: &mut Call<'_>, attr_p: u64) -> R<(String, Metadata)> {
    let a = c.g.obj_attr(attr_p)?.ok_or(Status(st::INVALID_PARAMETER))?;
    let nt = full_nt_name(c, a.root, &a.name)?;
    let r = stat_attr_inner(c, &nt);
    if FILE_HOOK.get().is_some() {
        let dir = r.as_ref().is_ok_and(|(_, m)| m.file_type == FileType::Directory);
        note_access(c, &nt, false, false, dir, r.is_ok());
    }
    r
}

fn stat_attr_inner(c: &mut Call<'_>, nt: &[u16]) -> R<(String, Metadata)> {
    let nt = nt.to_vec();
    match nt_to_target(c, &nt)? {
        Target::Path(p) => {
            let path = resolve_ci(c, &p).map_err(|parent| {
                Status(if parent { st::OBJECT_NAME_NOT_FOUND } else { st::OBJECT_PATH_NOT_FOUND })
            })?;
            let md = c.kernel().vfs().metadata(&path)?;
            Ok((path, md))
        }
        Target::Null => Ok((String::new(), console_md())),
        // A pipe exists while its server end is open.
        Target::Pipe(name) => match c.p.pipes.exists(&name) {
            true => Ok((String::new(), console_md())),
            false => Err(Status(st::OBJECT_NAME_NOT_FOUND)),
        },
        Target::Unsupported(s) => Err(c.unimplemented(&format!("{} of the NT path {s:?}", c.name()))),
    }
}

/// `NtQueryVolumeInformationFile(handle, iosb, buffer, len, class)`.
pub fn nt_query_volume_information_file(c: &mut Call<'_>) -> R {
    let (h, iosb, buf, len, class) = (c.a32(0), c.a(1), c.a(2), c.a32(3) as u64, c.a32(4));
    let f = get_file(c, h)?;
    let need = |n: u64| if len < n { Err(Status(st::INFO_LENGTH_MISMATCH)) } else { Ok(()) };
    let info = match class {
        // FileFsVolumeInformation: creation time, serial, label length,
        // supports objects, label.
        1 => {
            need(24)?;
            let label = to_w(if f.path.contains("/drive_c") { "gex" } else { "" });
            c.g.w64(buf, 0)?;
            c.g.w32(buf + 8, 0x6765_7801)?;
            c.g.w32(buf + 12, label.len() as u32 * 2)?;
            c.g.w8(buf + 16, 0)?;
            c.g.w8(buf + 17, 0)?;
            let room = ((len - 18) / 2) as usize;
            c.g.write_wchars(buf + 18, &label[..label.len().min(room)])?;
            18 + label.len().min(room) as u64 * 2
        }
        // FileFsSizeInformation: total, free, sectors per unit, bytes per
        // sector. gex does not track disk space; these are what an empty
        // 64 GiB disk reports.
        3 => {
            need(24)?;
            c.g.w64(buf, 16 << 20)?;
            c.g.w64(buf + 8, 8 << 20)?;
            c.g.w32(buf + 16, 8)?;
            c.g.w32(buf + 20, 512)?;
            24
        }
        // FileFsDeviceInformation: type, characteristics.
        4 => {
            need(8)?;
            let (ty, ch) = match f.kind {
                FileKind::Disk => (7u32, 0u32), // FILE_DEVICE_DISK
                FileKind::Console => (std_device_type(&f), 0),
                FileKind::Null => (0x15, 0), // FILE_DEVICE_NULL
                FileKind::Pipe => (0x11, 0), // FILE_DEVICE_NAMED_PIPE
            };
            c.g.w32(buf, ty)?;
            c.g.w32(buf + 4, ch)?;
            8
        }
        // FileFsAttributeInformation: attributes, max component length,
        // fs name.
        5 => {
            need(12)?;
            let name = to_w("NTFS");
            // FILE_CASE_PRESERVED_NAMES | FILE_UNICODE_ON_DISK
            c.g.w32(buf, 0x2 | 0x4)?;
            c.g.w32(buf + 4, 255)?;
            c.g.w32(buf + 8, name.len() as u32 * 2)?;
            let room = ((len - 12) / 2) as usize;
            c.g.write_wchars(buf + 12, &name[..name.len().min(room)])?;
            12 + name.len().min(room) as u64 * 2
        }
        // FileFsFullSizeInformation
        7 => {
            need(32)?;
            c.g.w64(buf, 16 << 20)?;
            c.g.w64(buf + 8, 8 << 20)?;
            c.g.w64(buf + 16, 8 << 20)?;
            c.g.w32(buf + 24, 8)?;
            c.g.w32(buf + 28, 512)?;
            32
        }
        _ => return Err(c.unimplemented(&format!("NtQueryVolumeInformationFile class {class}"))),
    };
    c.g.iosb(iosb, st::SUCCESS, info)?;
    Ok(st::SUCCESS)
}

/// The device type of a standard stream, from what the gex fd really is,
/// as Wine's `get_device_info` reports a Unix fd: a character device is
/// `FILE_DEVICE_UNKNOWN`, a pipe `FILE_DEVICE_NAMED_PIPE`, anything else a
/// disk. There is no console driver, so a stream is never a console.
fn std_device_type(f: &NtFile) -> u32 {
    match f.handle.metadata().map(|m| m.file_type) {
        Ok(FileType::CharDevice) => 0x22,
        Ok(FileType::Fifo) | Ok(FileType::Socket) => 0x11,
        _ => 7,
    }
}

/// Windows file-name matching, with the DOS wildcards `<`, `>` and `"`.
pub fn wild_match(mask: &[u16], name: &[u16]) -> bool {
    fn up(c: u16) -> u16 {
        if (b'a' as u16..=b'z' as u16).contains(&c) { c - 32 } else { c }
    }
    fn m(p: &[u16], n: &[u16]) -> bool {
        let Some((&pc, pr)) = p.split_first() else { return n.is_empty() };
        match pc {
            // `*`: any run.
            0x2a => (0..=n.len()).any(|i| m(pr, &n[i..])),
            // `<` (DOS_STAR): any run not containing the last dot.
            0x3c => {
                let last_dot = n.iter().rposition(|&c| c == b'.' as u16);
                (0..=n.len()).any(|i| last_dot.is_none_or(|d| i <= d) && m(pr, &n[i..]))
            }
            // `?`: one char. `>` (DOS_QM): one char, or nothing at a dot or
            // the end.
            0x3f => !n.is_empty() && m(pr, &n[1..]),
            0x3e => (!n.is_empty() && n[0] != b'.' as u16 && m(pr, &n[1..])) || m(pr, n),
            // `"` (DOS_DOT): a dot, or the end.
            0x22 => (n.first() == Some(&(b'.' as u16)) && m(pr, &n[1..])) || (n.is_empty() && m(pr, n)),
            c => n.first().is_some_and(|&x| up(x) == up(c)) && m(pr, &n[1..]),
        }
    }
    m(mask, name)
}

/// `NtQueryDirectoryFile(handle, event, apc, apc_ctx, iosb, buffer, len,
/// class, single, mask, restart)`.
pub fn nt_query_directory_file(c: &mut Call<'_>) -> R {
    let (h, event, apc, iosb, buf, len) = (c.a32(0), c.a32(1), c.a(2), c.a(4), c.a(5), c.a32(6) as u64);
    let (class, single, mask_p, restart) = (c.a32(7), c.a32(8) & 0xff != 0, c.a(9), c.a32(10) & 0xff != 0);
    let f = get_file(c, h)?;
    if !f.is_dir {
        return Err(Status(st::INVALID_PARAMETER));
    }
    // Fixed part size and where the name goes, per class.
    let (fixed, name_len_at, name_at, short_at) = match class {
        1 => (64u64, 60u64, 64u64, None),                // FileDirectoryInformation
        2 => (68, 60, 68, None),                         // FileFullDirectoryInformation
        3 => (94, 60, 94, Some(68u64)),                  // FileBothDirectoryInformation
        12 => (12, 8, 12, None),                         // FileNamesInformation
        37 => (104, 60, 104, Some(68)),                  // FileIdBothDirectoryInformation
        38 => (80, 60, 80, None),                        // FileIdFullDirectoryInformation
        _ => return Err(c.unimplemented(&format!("NtQueryDirectoryFile class {class}"))),
    };
    let mask = c.g.ustr(mask_p)?;
    let mut scan = f.dir.lock().unwrap();
    if scan.is_none() || restart {
        let mut entries = Vec::new();
        let listing = f.handle.read_dir()?;
        let vfs = c.kernel().vfs();
        for e in listing {
            let p = if e.name == "." || e.name == ".." { f.path.clone() } else { format!("{}/{}", f.path, e.name) };
            if let Ok(md) = vfs.metadata(&p) {
                entries.push((e.name, md));
            }
        }
        drop(vfs);
        let keep_mask = scan.as_ref().and_then(|s| s.mask.clone());
        *scan = Some(DirScan { entries, next: 0, mask: mask.clone().or(keep_mask) });
    } else if let (Some(m), Some(s)) = (mask.clone(), scan.as_mut()) {
        if s.next == 0 {
            s.mask = Some(m);
        }
    }
    let s = scan.as_mut().unwrap();
    let mut at = 0u64;
    let mut last_entry: Option<u64> = None;
    let mut wrote = 0;
    while s.next < s.entries.len() {
        let (name, md) = s.entries[s.next].clone();
        let wname = to_w(&name);
        if let Some(m) = &s.mask {
            if !wild_match(m, &wname) {
                s.next += 1;
                continue;
            }
        }
        let size = (fixed + wname.len() as u64 * 2 + 7) & !7;
        let need = fixed + wname.len() as u64 * 2;
        if at + need > len {
            if wrote == 0 {
                drop(scan);
                return Err(Status(st::BUFFER_OVERFLOW));
            }
            break;
        }
        let e = buf + at;
        c.g.zero(e, fixed as usize)?;
        c.g.w32(e + 4, s.next as u32)?; // FileIndex
        if class != 12 {
            let t = filetime(md.mtime);
            for i in 0..4 {
                c.g.w64(e + 8 + 8 * i, t)?;
            }
            let sz = if md.file_type == FileType::Directory { 0 } else { md.size };
            c.g.w64(e + 40, sz)?;
            c.g.w64(e + 48, (sz + 4095) & !4095)?;
            c.g.w32(e + 56, file_attributes(&md, &name))?;
        }
        c.g.w32(e + name_len_at, wname.len() as u32 * 2)?;
        if let Some(sa) = short_at {
            // ShortNameLength = 0: no 8.3 names.
            c.g.w8(e + sa, 0)?;
        }
        if class == 37 || class == 38 {
            c.g.w64(e + name_at - 8, md.ino)?;
        }
        c.g.write_wchars(e + name_at, &wname)?;
        if let Some(prev) = last_entry {
            c.g.w32(buf + prev, (at - prev) as u32)?;
        }
        last_entry = Some(at);
        at += size;
        s.next += 1;
        wrote += 1;
        if single {
            break;
        }
    }
    let first_call_empty = wrote == 0;
    let used = last_entry.map(|l| {
        let (n, _) = &s.entries[s.next - 1];
        l + fixed + to_w(n).len() as u64 * 2
    });
    drop(scan);
    if first_call_empty {
        let status = if mask.is_some() && restart { st::NO_SUCH_FILE } else { st::NO_MORE_FILES };
        c.g.iosb(iosb, status, 0)?;
        return Err(Status(status));
    }
    complete_io(c, event, apc, iosb, st::SUCCESS, used.unwrap_or(0))?;
    Ok(st::SUCCESS)
}

const FSCTL_GET_OBJECT_ID: u32 = 0x9009c;
const FILE_DEVICE_CONSOLE: u32 = 0x50;

/// `NtFsControlFile(handle, event, apc, apc_ctx, iosb, code, in, in_len,
/// out, out_len)`.
pub fn nt_fs_control_file(c: &mut Call<'_>) -> R {
    let (h, event, apc, iosb, code, out, out_len) = (c.a32(0), c.a32(1), c.a(2), c.a(4), c.a32(5), c.a(8), c.a32(9));
    let f = get_file(c, h)?;
    match code {
        // FILE_OBJECTID_BUFFER: an id that is the same for two names of one
        // file (the loader uses it to spot a DLL loaded twice). Wine uses
        // st_dev and st_ino; gex's VFS has the inode.
        FSCTL_GET_OBJECT_ID => {
            if out_len < 64 {
                return Err(Status(st::BUFFER_TOO_SMALL));
            }
            let md = f.handle.metadata()?;
            c.g.zero(out, 64)?;
            c.g.w64(out, 0x6765_7876_6673)?; // "gexvfs": the one device
            c.g.w64(out + 8, md.ino)?;
            complete_io(c, event, apc, iosb, st::SUCCESS, 64)?;
            Ok(st::SUCCESS)
        }
        _ => Err(c.unimplemented(&format!("NtFsControlFile code {code:#x}"))),
    }
}

/// `NtDeviceIoControlFile(handle, event, apc, apc_ctx, iosb, code, in,
/// in_len, out, out_len)`.
pub fn nt_device_io_control_file(c: &mut Call<'_>) -> R {
    let (h, code) = (c.a32(0), c.a32(5));
    let f = get_file(c, h)?;
    // A console-driver request on a stream that is not a console (gex has no
    // console driver): wineserver's `default_fd_ioctl` answers a plain fd
    // with STATUS_NOT_SUPPORTED, which is how kernelbase learns that
    // `GetConsoleMode` does not apply.
    let _ = &f;
    if code >> 16 == FILE_DEVICE_CONSOLE {
        return Err(Status(st::NOT_SUPPORTED));
    }
    Err(c.unimplemented(&format!("NtDeviceIoControlFile code {code:#x}")))
}

/// `NtFlushBuffersFile(handle, iosb)`: writes are not buffered in gex.
pub fn nt_flush_buffers_file(c: &mut Call<'_>) -> R {
    let (h, iosb) = (c.a32(0), c.a(1));
    get_file(c, h)?;
    c.g.iosb(iosb, st::SUCCESS, 0)?;
    Ok(st::SUCCESS)
}

/// Close-time work for a file: delete-on-close.
pub fn on_last_close(vfs: &crate::vfs::Namespace, f: &NtFile) {
    if f.delete_on_close.load(Ordering::Relaxed) && f.kind == FileKind::Disk {
        let path = f.moved_to.lock().unwrap().clone().unwrap_or_else(|| f.path.clone());
        let _ = if f.is_dir { vfs.rmdir(&path) } else { vfs.unlink(&path) };
    }
}

/// A console stream file for the process's fd `fd`.
pub fn console_file(k: &crate::Kernel, fd: i32, access: u32) -> Option<Arc<NtFile>> {
    let of = k.fds.get(fd).ok()?;
    Some(Arc::new(NtFile {
        handle: Arc::clone(&of.handle),
        path: String::new(),
        nt_path: to_w(match fd {
            0 => "\\Device\\ConDrv\\Input",
            _ => "\\Device\\ConDrv\\Output",
        }),
        pos: AtomicU64::new(0),
        options: opt::SYNCHRONOUS_IO_NONALERT,
        access,
        is_dir: false,
        kind: FileKind::Console,
        delete_on_close: AtomicBool::new(false),
        dir: Mutex::new(None),
        notify: Mutex::new(None),
        moved_to: Mutex::new(None),
    }))
}

/// Read a whole file by guest path (NLS files, images).
pub fn read_guest_file(ns: &crate::vfs::Namespace, path: &str) -> Result<Vec<u8>, crate::Errno> {
    let h = ns.open(path, oflags::O_RDONLY, 0);
    if let Some(hook) = FILE_HOOK.get() {
        hook(&FileAccess { path, read: true, write: false, create: false, directory: false, ok: h.is_ok() });
    }
    let h = h?;
    let size = h.metadata()?.size as usize;
    let mut v = vec![0u8; size];
    let mut done = 0;
    while done < size {
        let n = h.read_at(done as u64, &mut v[done..])? as usize;
        if n == 0 {
            break;
        }
        done += n;
    }
    v.truncate(done);
    Ok(v)
}

#[allow(dead_code)]
fn abi_unused(_: Abi) {}

#[cfg(test)]
mod tests {
    use super::wild_match;
    fn w(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }
    #[test]
    fn wildcards() {
        assert!(wild_match(&w("*"), &w("kernel32.dll")));
        assert!(wild_match(&w("*.DLL"), &w("kernel32.dll")));
        assert!(!wild_match(&w("*.exe"), &w("kernel32.dll")));
        assert!(wild_match(&w("k?rnel32.dll"), &w("KERNEL32.DLL")));
        assert!(wild_match(&w("<.dll"), &w("a.b.dll")));
        assert!(wild_match(&w("foo\"*"), &w("foo")));
    }
}
