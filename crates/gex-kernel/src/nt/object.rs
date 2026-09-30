//! Objects, handles and the object namespace (docs/nt-layer.md §3.1).

use super::file::NtFile;
use super::reg::Key;
use super::section::Section;
use super::sync::SyncObj;
use super::{st, to_w, wstr, Call, Status, Thread, R};
use std::collections::HashMap;
use std::sync::Arc;

/// What a handle refers to.
#[derive(Clone, Debug)]
pub enum Obj {
    File(Arc<NtFile>),
    Section(Arc<Section>),
    /// Events, mutants, semaphores, keyed events.
    Sync(Arc<SyncObj>),
    Thread(Arc<Thread>),
    /// This process (the only one there is).
    Process,
    Key(Arc<Key>),
    /// An object directory, by its full name (`\KnownDlls`).
    Directory(String),
    Token,
}

impl Obj {
    pub fn type_name(&self) -> &'static str {
        match self {
            Obj::File(_) => "File",
            Obj::Section(_) => "Section",
            Obj::Sync(s) => s.kind.type_name(),
            Obj::Thread(_) => "Thread",
            Obj::Process => "Process",
            Obj::Key(_) => "Key",
            Obj::Directory(_) => "Directory",
            Obj::Token => "Token",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub obj: Obj,
    pub access: u32,
}

/// A process's handle table. Handles are multiples of 4, as on Windows.
#[derive(Debug)]
pub struct Handles {
    map: HashMap<u32, Entry>,
    next: u32,
    free: Vec<u32>,
    /// Named objects (`\BaseNamedObjects\foo`, lower-cased) → object.
    pub named: HashMap<String, Obj>,
}

impl Default for Handles {
    fn default() -> Self {
        Handles { map: HashMap::new(), next: 4, free: Vec::new(), named: HashMap::new() }
    }
}

/// `NtCurrentProcess()` and `NtCurrentThread()`.
pub const CURRENT_PROCESS: u32 = 0xffff_ffff;
pub const CURRENT_THREAD: u32 = 0xffff_fffe;

impl Handles {
    pub fn insert(&mut self, obj: Obj, access: u32) -> u32 {
        let h = self.free.pop().unwrap_or_else(|| {
            let h = self.next;
            self.next += 4;
            h
        });
        self.map.insert(h, Entry { obj, access });
        h
    }

    pub fn get(&self, h: u32) -> Option<&Entry> {
        self.map.get(&h)
    }

    pub fn remove(&mut self, h: u32) -> Option<Entry> {
        let e = self.map.remove(&h)?;
        self.free.push(h);
        Some(e)
    }

    /// Whether any handle still refers to the same file.
    pub fn file_still_open(&self, f: &Arc<NtFile>) -> bool {
        self.map.values().any(|e| matches!(&e.obj, Obj::File(g) if Arc::ptr_eq(f, g)))
    }
}

/// Resolve a handle, including the pseudo-handles.
pub fn resolve(c: &Call<'_>, h: u32) -> R<Obj> {
    match h {
        CURRENT_PROCESS => Ok(Obj::Process),
        CURRENT_THREAD => Ok(Obj::Thread(Arc::clone(&c.t))),
        _ => c
            .p
            .st
            .lock()
            .unwrap()
            .handles
            .get(h)
            .map(|e| e.obj.clone())
            .ok_or(Status(st::INVALID_HANDLE)),
    }
}

/// Close-time work for an object whose last handle just went.
fn closed(c: &Call<'_>, obj: &Obj) {
    if let Obj::File(f) = obj {
        let still = c.p.st.lock().unwrap().handles.file_still_open(f);
        if !still {
            super::file::on_last_close(&mut c.kernel(), f);
        }
    }
}

/// `NtClose(handle)`.
pub fn nt_close(c: &mut Call<'_>) -> R {
    let h = c.a32(0);
    if h == CURRENT_PROCESS || h == CURRENT_THREAD {
        return Ok(st::SUCCESS);
    }
    let e = c.p.st.lock().unwrap().handles.remove(h).ok_or(Status(st::INVALID_HANDLE))?;
    closed(c, &e.obj);
    Ok(st::SUCCESS)
}

/// `NtDuplicateObject(src_process, src, dst_process, *dst, access,
/// attributes, options)`, within this process.
pub fn nt_duplicate_object(c: &mut Call<'_>) -> R {
    let (sp, src, dp, dst_p, access, _attrs, options) =
        (c.a32(0), c.a32(1), c.a32(2), c.a(3), c.a32(4), c.a32(5), c.a32(6));
    const CLOSE_SOURCE: u32 = 1;
    const SAME_ACCESS: u32 = 2;
    for ph in [sp, dp] {
        if ph != 0 && !matches!(resolve(c, ph)?, Obj::Process) {
            return Err(c.unimplemented("NtDuplicateObject across processes"));
        }
    }
    let obj = resolve(c, src)?;
    let old_access = c.p.st.lock().unwrap().handles.get(src).map(|e| e.access).unwrap_or(0x1f_ffff);
    let access = if options & SAME_ACCESS != 0 { old_access } else { access };
    if dp == 0 || dst_p == 0 {
        // Only closing the source.
        if options & CLOSE_SOURCE != 0 {
            if let Some(e) = c.p.st.lock().unwrap().handles.remove(src) {
                closed(c, &e.obj);
            }
        }
        return Ok(st::SUCCESS);
    }
    let h = {
        let mut stl = c.p.st.lock().unwrap();
        let h = stl.handles.insert(obj, access);
        if options & CLOSE_SOURCE != 0 && src != CURRENT_PROCESS && src != CURRENT_THREAD {
            stl.handles.remove(src);
        }
        h
    };
    c.g.wptr(dst_p, h as u64)?;
    Ok(st::SUCCESS)
}

/// The object directories gex has. Empty ones answer "not found" for any
/// name inside, which is the truth.
const DIRECTORIES: &[&str] = &[
    "\\",
    "\\knowndlls",
    "\\knowndlls32",
    "\\basenamedobjects",
    "\\sessions",
    "\\sessions\\1",
    "\\sessions\\1\\basenamedobjects",
    "\\sessions\\1\\windows",
    "\\sessions\\1\\windows\\windowstations",
    "\\device",
    "\\??",
    "\\global??",
];

/// The full name of an object: `OBJECT_ATTRIBUTES` resolved against an
/// object-directory root.
pub fn object_name(c: &Call<'_>, root: u32, name: &[u16]) -> R<String> {
    let n = wstr(name);
    if root == 0 {
        return Ok(n);
    }
    match resolve(c, root)? {
        Obj::Directory(d) => Ok(if d == "\\" { format!("\\{n}") } else { format!("{d}\\{n}") }),
        _ => Err(Status(st::OBJECT_TYPE_MISMATCH)),
    }
}

/// `NtOpenDirectoryObject(*handle, access, attr)`.
pub fn nt_open_directory_object(c: &mut Call<'_>) -> R {
    let (hp, access, attr_p) = (c.a(0), c.a32(1), c.a(2));
    let a = c.g.obj_attr(attr_p)?.ok_or(Status(st::INVALID_PARAMETER))?;
    let name = object_name(c, a.root, &a.name)?;
    let lower = name.to_ascii_lowercase();
    if !DIRECTORIES.contains(&lower.as_str()) {
        return Err(Status(st::OBJECT_NAME_NOT_FOUND));
    }
    let h = c.p.st.lock().unwrap().handles.insert(Obj::Directory(lower), access);
    c.g.wptr(hp, h as u64)?;
    Ok(st::SUCCESS)
}

/// `NtOpenSymbolicLinkObject`: gex's namespace has no symbolic links yet.
pub fn nt_open_symbolic_link_object(c: &mut Call<'_>) -> R {
    let a = c.g.obj_attr(c.a(2))?.ok_or(Status(st::INVALID_PARAMETER))?;
    let name = object_name(c, a.root, &a.name)?;
    Err(c.unimplemented(&format!("NtOpenSymbolicLinkObject {name:?}")))
}

/// `NtQueryObject(handle, class, buffer, len, *ret_len)`.
pub fn nt_query_object(c: &mut Call<'_>) -> R {
    let (h, class, buf, len, ret) = (c.a32(0), c.a32(1), c.a(2), c.a32(3) as u64, c.a(4));
    let obj = resolve(c, h)?;
    match class {
        // ObjectBasicInformation: attributes, access, handle count, pointer
        // count, then reserved fields (56 bytes).
        0 => {
            if len < 56 {
                return Err(Status(st::INFO_LENGTH_MISMATCH));
            }
            let access = c.p.st.lock().unwrap().handles.get(h).map(|e| e.access).unwrap_or(0x1f_ffff);
            c.g.zero(buf, 56)?;
            c.g.w32(buf + 4, access)?;
            c.g.w32(buf + 8, 1)?;
            c.g.w32(buf + 12, 1)?;
            c.g.out32(ret, 56)?;
            Ok(st::SUCCESS)
        }
        // ObjectNameInformation: a UNICODE_STRING followed by the name.
        1 => {
            let name = match &obj {
                Obj::File(f) => f.nt_path.clone(),
                Obj::Directory(d) => to_w(d),
                _ => Vec::new(),
            };
            let us = c.g.us_size();
            let need = us + name.len() as u64 * 2 + 2;
            c.g.out32(ret, need as u32)?;
            if len < need {
                return Err(Status(if len < us { st::INFO_LENGTH_MISMATCH } else { st::BUFFER_OVERFLOW }));
            }
            let bytes = name.len() as u16 * 2;
            c.g.w_ustr_hdr(buf, bytes, bytes + 2, if name.is_empty() { 0 } else { buf + us })?;
            let mut z = name.clone();
            z.push(0);
            c.g.write_wchars(buf + us, &z)?;
            Ok(st::SUCCESS)
        }
        // ObjectTypeInformation: the type name first.
        2 => {
            let name = to_w(obj.type_name());
            let us = c.g.us_size();
            // OBJECT_TYPE_INFORMATION is 0x68 (64-bit) / 0x60 (32-bit) bytes.
            let fixed = if c.g.p64() { 0x68 } else { 0x60 };
            let need = fixed + name.len() as u64 * 2 + 2;
            c.g.out32(ret, need as u32)?;
            if len < need {
                return Err(Status(st::INFO_LENGTH_MISMATCH));
            }
            c.g.zero(buf, fixed as usize)?;
            let bytes = name.len() as u16 * 2;
            c.g.w_ustr_hdr(buf, bytes, bytes + 2, buf + fixed)?;
            let mut z = name;
            z.push(0);
            c.g.write_wchars(buf + fixed, &z)?;
            let _ = us;
            Ok(st::SUCCESS)
        }
        _ => Err(c.unimplemented(&format!("NtQueryObject class {class}"))),
    }
}
