//! The registry: an in-memory key tree (docs/nt-layer.md §10).
//!
//! Keys live under `\Registry\Machine` and `\Registry\User\<sid>`. The tree
//! starts with the keys Wine's PE side expects to exist and is not persisted
//! yet; a missing key or value is answered "not found", which is what an empty
//! prefix answers too.

use super::object::Obj;
use super::{st, to_w, wstr, Call, Status, R};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

pub mod types {
    pub const NONE: u32 = 0;
    pub const SZ: u32 = 1;
    pub const EXPAND_SZ: u32 = 2;
    pub const BINARY: u32 = 3;
    pub const DWORD: u32 = 4;
    pub const MULTI_SZ: u32 = 7;
    pub const QWORD: u32 = 11;
}

#[derive(Debug, Clone)]
pub struct Value {
    pub name: Vec<u16>,
    pub ty: u32,
    pub data: Vec<u8>,
}

#[derive(Debug)]
pub struct Key {
    /// The last path component, as created.
    pub name: Vec<u16>,
    /// The full path, `\Registry\Machine\…`.
    pub path: String,
    pub class: Mutex<Vec<u16>>,
    pub children: Mutex<BTreeMap<String, Arc<Key>>>,
    pub values: Mutex<Vec<Value>>,
}

impl Key {
    fn new(name: &str, path: String) -> Arc<Key> {
        Arc::new(Key {
            name: to_w(name),
            path,
            class: Mutex::new(Vec::new()),
            children: Mutex::new(BTreeMap::new()),
            values: Mutex::new(Vec::new()),
        })
    }

    fn child(&self, name: &str) -> Option<Arc<Key>> {
        self.children.lock().unwrap().get(&name.to_lowercase()).cloned()
    }

    fn child_or_create(&self, name: &str) -> (Arc<Key>, bool) {
        let mut ch = self.children.lock().unwrap();
        if let Some(k) = ch.get(&name.to_lowercase()) {
            return (Arc::clone(k), false);
        }
        let k = Key::new(name, format!("{}\\{}", self.path, name));
        ch.insert(name.to_lowercase(), Arc::clone(&k));
        (k, true)
    }

    pub fn set(&self, name: &str, ty: u32, data: Vec<u8>) {
        let wname = to_w(name);
        let mut vs = self.values.lock().unwrap();
        match vs.iter_mut().find(|v| eq_ci(&v.name, &wname)) {
            Some(v) => {
                v.ty = ty;
                v.data = data;
            }
            None => vs.push(Value { name: wname, ty, data }),
        }
    }

    pub fn set_sz(&self, name: &str, s: &str) {
        let mut d: Vec<u8> = to_w(s).iter().flat_map(|c| c.to_le_bytes()).collect();
        d.extend_from_slice(&[0, 0]);
        self.set(name, types::SZ, d);
    }

    pub fn set_dword(&self, name: &str, v: u32) {
        self.set(name, types::DWORD, v.to_le_bytes().to_vec());
    }
}

fn eq_ci(a: &[u16], b: &[u16]) -> bool {
    wstr(a).to_lowercase() == wstr(b).to_lowercase()
}

/// The user's SID, which `\Registry\User\<sid>` (HKCU) is named after and
/// `NtQueryInformationToken(TokenUser)` reports.
pub const USER_SID: &str = "S-1-5-21-0-0-0-1000";

#[derive(Debug)]
pub struct Registry {
    pub root: Arc<Key>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub fn new() -> Registry {
        let r = Registry { root: Key::new("Registry", "\\Registry".into()) };
        r.seed();
        r
    }

    /// Find or create a path below the root (`Machine\Software\…`).
    pub fn create(&self, path: &str) -> Arc<Key> {
        let mut k = Arc::clone(&self.root);
        for comp in path.split('\\').filter(|s| !s.is_empty()) {
            k = k.child_or_create(comp).0;
        }
        k
    }

    fn seed(&self) {
        self.create("Machine\\Software\\Classes");
        self.create("Machine\\Software\\Microsoft\\Windows NT\\CurrentVersion");
        self.create("Machine\\System\\CurrentControlSet\\Control\\Session Manager\\Environment");
        self.create(&format!("User\\{USER_SID}\\Software"));
        self.create(&format!("User\\{USER_SID}\\Environment"));
        let cv = self.create("Machine\\Software\\Microsoft\\Windows NT\\CurrentVersion");
        cv.set_sz("CurrentVersion", "6.3");
        cv.set_sz("CurrentBuild", "19045");
        cv.set_sz("CurrentBuildNumber", "19045");
        cv.set_dword("CurrentMajorVersionNumber", 10);
        cv.set_dword("CurrentMinorVersionNumber", 0);
        cv.set_sz("ProductName", "Windows 10 Pro");
        cv.set_sz("CSDVersion", "");
        cv.set_sz("SystemRoot", "C:\\windows");
    }
}

/// Resolve `OBJECT_ATTRIBUTES` to (parent key, remaining components).
fn split_path(c: &Call<'_>, root: u32, name: &[u16]) -> R<(Arc<Key>, Vec<String>)> {
    let n = wstr(name);
    let base = if root == 0 {
        let lower = n.to_ascii_lowercase();
        if !lower.starts_with("\\registry") {
            return Err(Status(st::OBJECT_PATH_SYNTAX_BAD));
        }
        (Arc::clone(&c.p.reg.root), n["\\registry".len()..].to_string())
    } else {
        match super::object::resolve(c, root)? {
            Obj::Key(k) => (k, n),
            _ => return Err(Status(st::OBJECT_TYPE_MISMATCH)),
        }
    };
    let comps = base.1.split('\\').filter(|s| !s.is_empty()).map(str::to_string).collect();
    Ok((base.0, comps))
}

fn get_key(c: &Call<'_>, h: u32) -> R<Arc<Key>> {
    match super::object::resolve(c, h)? {
        Obj::Key(k) => Ok(k),
        _ => Err(Status(st::OBJECT_TYPE_MISMATCH)),
    }
}

fn open_key(c: &mut Call<'_>, hp: u64, access: u32, attr_p: u64) -> R {
    let a = c.g.obj_attr(attr_p)?.ok_or(Status(st::INVALID_PARAMETER))?;
    let (mut k, comps) = split_path(c, a.root, &a.name)?;
    for comp in &comps {
        k = k.child(comp).ok_or(Status(st::OBJECT_NAME_NOT_FOUND))?;
    }
    let h = c.p.st.lock().unwrap().handles.insert(Obj::Key(k), access);
    c.g.wptr(hp, h as u64)?;
    Ok(st::SUCCESS)
}

/// `NtCreateKey(*handle, access, attr, index, class, options, *disposition)`.
pub fn nt_create_key(c: &mut Call<'_>) -> R {
    let (hp, access, attr_p, class_p, disp_p) = (c.a(0), c.a32(1), c.a(2), c.a(4), c.a(6));
    let a = c.g.obj_attr(attr_p)?.ok_or(Status(st::INVALID_PARAMETER))?;
    let (mut k, comps) = split_path(c, a.root, &a.name)?;
    let mut created = false;
    for comp in &comps {
        let (next, new) = k.child_or_create(comp);
        k = next;
        created = new;
    }
    if created {
        if let Some(class) = c.g.ustr(class_p)? {
            *k.class.lock().unwrap() = class;
        }
    }
    let h = c.p.st.lock().unwrap().handles.insert(Obj::Key(k), access);
    c.g.wptr(hp, h as u64)?;
    // REG_CREATED_NEW_KEY / REG_OPENED_EXISTING_KEY
    c.g.out32(disp_p, if created { 1 } else { 2 })?;
    Ok(st::SUCCESS)
}

/// `NtOpenKey(*handle, access, attr)`.
pub fn nt_open_key(c: &mut Call<'_>) -> R {
    let (hp, access, attr_p) = (c.a(0), c.a32(1), c.a(2));
    open_key(c, hp, access, attr_p)
}

/// `NtOpenKeyEx(*handle, access, attr, options)`.
pub fn nt_open_key_ex(c: &mut Call<'_>) -> R {
    let (hp, access, attr_p) = (c.a(0), c.a32(1), c.a(2));
    open_key(c, hp, access, attr_p)
}

/// Write a `KEY_VALUE_*_INFORMATION`. Returns the status.
fn write_value_info(c: &mut Call<'_>, v: &Value, class: u32, buf: u64, len: u64, ret_p: u64) -> R {
    let name_b = v.name.len() as u64 * 2;
    let dlen = v.data.len() as u64;
    // (fixed header, where the name goes, where the data goes)
    let (fixed, total) = match class {
        0 => (12u64, 12 + name_b),                           // Basic
        1 => (20u64, (20 + name_b + 7) / 8 * 8 + dlen),      // Full: data 8-aligned
        2 => (12u64, 12 + dlen),                             // Partial
        _ => return Err(c.unimplemented(&format!("KeyValueInformation class {class}"))),
    };
    c.g.out32(ret_p, total as u32)?;
    if len < fixed {
        return Err(Status(st::BUFFER_TOO_SMALL));
    }
    c.g.w32(buf, 0)?; // TitleIndex
    c.g.w32(buf + 4, v.ty)?;
    let mut full = true;
    match class {
        0 => {
            c.g.w32(buf + 8, name_b as u32)?;
            let room = ((len - fixed) / 2) as usize;
            full = room >= v.name.len();
            c.g.write_wchars(buf + 12, &v.name[..v.name.len().min(room)])?;
        }
        1 => {
            let data_off = (20 + name_b + 7) / 8 * 8;
            c.g.w32(buf + 8, data_off as u32)?;
            c.g.w32(buf + 12, dlen as u32)?;
            c.g.w32(buf + 16, name_b as u32)?;
            let room = ((len - fixed) / 2) as usize;
            c.g.write_wchars(buf + 20, &v.name[..v.name.len().min(room)])?;
            if len >= data_off + dlen {
                c.g.write(buf + data_off, &v.data)?;
            } else {
                full = false;
            }
        }
        _ => {
            c.g.w32(buf + 8, dlen as u32)?;
            let room = (len - fixed) as usize;
            full = room >= v.data.len();
            c.g.write(buf + 12, &v.data[..v.data.len().min(room)])?;
        }
    }
    if full {
        Ok(st::SUCCESS)
    } else {
        Err(Status(st::BUFFER_OVERFLOW))
    }
}

/// `NtQueryValueKey(handle, name, class, buffer, len, *ret_len)`.
pub fn nt_query_value_key(c: &mut Call<'_>) -> R {
    let (h, name_p, class, buf, len, ret) = (c.a32(0), c.a(1), c.a32(2), c.a(3), c.a32(4) as u64, c.a(5));
    let k = get_key(c, h)?;
    let name = c.g.ustr(name_p)?.unwrap_or_default();
    let v = k.values.lock().unwrap().iter().find(|v| eq_ci(&v.name, &name)).cloned();
    let v = v.ok_or(Status(st::OBJECT_NAME_NOT_FOUND))?;
    write_value_info(c, &v, class, buf, len, ret)
}

/// `NtEnumerateValueKey(handle, index, class, buffer, len, *ret_len)`.
pub fn nt_enumerate_value_key(c: &mut Call<'_>) -> R {
    let (h, index, class, buf, len, ret) = (c.a32(0), c.a32(1), c.a32(2), c.a(3), c.a32(4) as u64, c.a(5));
    let k = get_key(c, h)?;
    let v = k.values.lock().unwrap().get(index as usize).cloned();
    let v = v.ok_or(Status(st::NO_MORE_ENTRIES))?;
    write_value_info(c, &v, class, buf, len, ret)
}

/// `NtSetValueKey(handle, name, index, type, data, len)`.
pub fn nt_set_value_key(c: &mut Call<'_>) -> R {
    let (h, name_p, ty, data_p, len) = (c.a32(0), c.a(1), c.a32(3), c.a(4), c.a32(5) as usize);
    let k = get_key(c, h)?;
    let name = c.g.ustr(name_p)?.unwrap_or_default();
    let data = c.g.bytes(data_p, len)?;
    k.set(&wstr(&name), ty, data);
    Ok(st::SUCCESS)
}

/// `NtDeleteValueKey(handle, name)`.
pub fn nt_delete_value_key(c: &mut Call<'_>) -> R {
    let (h, name_p) = (c.a32(0), c.a(1));
    let k = get_key(c, h)?;
    let name = c.g.ustr(name_p)?.unwrap_or_default();
    let mut vs = k.values.lock().unwrap();
    let before = vs.len();
    vs.retain(|v| !eq_ci(&v.name, &name));
    if vs.len() == before {
        return Err(Status(st::OBJECT_NAME_NOT_FOUND));
    }
    Ok(st::SUCCESS)
}

/// Write a `KEY_*_INFORMATION` for key `k`, naming it by `name`.
fn write_key_info(c: &mut Call<'_>, k: &Key, name: &[u16], class: u32, buf: u64, len: u64, ret_p: u64) -> R {
    let name_b = name.len() as u64 * 2;
    let kclass = k.class.lock().unwrap().clone();
    let class_b = kclass.len() as u64 * 2;
    let children = k.children.lock().unwrap();
    let values = k.values.lock().unwrap();
    match class {
        // KeyBasicInformation: LastWriteTime, TitleIndex, NameLength, Name.
        0 => {
            let total = 16 + name_b;
            c.g.out32(ret_p, total as u32)?;
            if len < 16 {
                return Err(Status(st::BUFFER_TOO_SMALL));
            }
            c.g.w64(buf, 0)?;
            c.g.w32(buf + 8, 0)?;
            c.g.w32(buf + 12, name_b as u32)?;
            let room = ((len - 16) / 2) as usize;
            c.g.write_wchars(buf + 16, &name[..name.len().min(room)])?;
            if room < name.len() { Err(Status(st::BUFFER_OVERFLOW)) } else { Ok(st::SUCCESS) }
        }
        // KeyNodeInformation: …, ClassOffset, ClassLength, NameLength, Name, Class.
        1 => {
            let total = 24 + name_b + class_b;
            c.g.out32(ret_p, total as u32)?;
            if len < 24 {
                return Err(Status(st::BUFFER_TOO_SMALL));
            }
            c.g.w64(buf, 0)?;
            c.g.w32(buf + 8, 0)?;
            c.g.w32(buf + 12, if class_b > 0 { (24 + name_b) as u32 } else { u32::MAX })?;
            c.g.w32(buf + 16, class_b as u32)?;
            c.g.w32(buf + 20, name_b as u32)?;
            if len < total {
                return Err(Status(st::BUFFER_OVERFLOW));
            }
            c.g.write_wchars(buf + 24, name)?;
            c.g.write_wchars(buf + 24 + name_b, &kclass)?;
            Ok(st::SUCCESS)
        }
        // KeyFullInformation.
        2 => {
            let total = 44 + class_b;
            c.g.out32(ret_p, total as u32)?;
            if len < 44 {
                return Err(Status(st::BUFFER_TOO_SMALL));
            }
            c.g.w64(buf, 0)?;
            c.g.w32(buf + 8, 0)?;
            c.g.w32(buf + 12, if class_b > 0 { 44 } else { u32::MAX })?;
            c.g.w32(buf + 16, class_b as u32)?;
            c.g.w32(buf + 20, children.len() as u32)?;
            c.g.w32(buf + 24, children.values().map(|k| k.name.len() * 2).max().unwrap_or(0) as u32)?;
            c.g.w32(buf + 28, children.values().map(|k| k.class.lock().unwrap().len() * 2).max().unwrap_or(0) as u32)?;
            c.g.w32(buf + 32, values.len() as u32)?;
            c.g.w32(buf + 36, values.iter().map(|v| v.name.len() * 2).max().unwrap_or(0) as u32)?;
            c.g.w32(buf + 40, values.iter().map(|v| v.data.len()).max().unwrap_or(0) as u32)?;
            if len < total {
                return Err(Status(st::BUFFER_OVERFLOW));
            }
            c.g.write_wchars(buf + 44, &kclass)?;
            Ok(st::SUCCESS)
        }
        // KeyNameInformation: NameLength, Name (the full path).
        3 => {
            let full = to_w(&k.path);
            let total = 4 + full.len() as u64 * 2;
            c.g.out32(ret_p, total as u32)?;
            if len < 4 {
                return Err(Status(st::BUFFER_TOO_SMALL));
            }
            c.g.w32(buf, full.len() as u32 * 2)?;
            if len < total {
                return Err(Status(st::BUFFER_OVERFLOW));
            }
            c.g.write_wchars(buf + 4, &full)?;
            Ok(st::SUCCESS)
        }
        // KeyCachedInformation.
        4 => {
            c.g.out32(ret_p, 40)?;
            if len < 40 {
                return Err(Status(st::BUFFER_TOO_SMALL));
            }
            c.g.w64(buf, 0)?;
            c.g.w32(buf + 8, 0)?;
            c.g.w32(buf + 12, children.len() as u32)?;
            c.g.w32(buf + 16, children.values().map(|k| k.name.len() * 2).max().unwrap_or(0) as u32)?;
            c.g.w32(buf + 20, values.len() as u32)?;
            c.g.w32(buf + 24, values.iter().map(|v| v.name.len() * 2).max().unwrap_or(0) as u32)?;
            c.g.w32(buf + 28, values.iter().map(|v| v.data.len()).max().unwrap_or(0) as u32)?;
            c.g.w32(buf + 32, name_b as u32)?;
            c.g.w32(buf + 36, 0)?;
            Ok(st::SUCCESS)
        }
        _ => Err(c.unimplemented(&format!("KeyInformation class {class}"))),
    }
}

/// `NtEnumerateKey(handle, index, class, buffer, len, *ret_len)`.
pub fn nt_enumerate_key(c: &mut Call<'_>) -> R {
    let (h, index, class, buf, len, ret) = (c.a32(0), c.a32(1), c.a32(2), c.a(3), c.a32(4) as u64, c.a(5));
    let k = get_key(c, h)?;
    let child = k.children.lock().unwrap().values().nth(index as usize).cloned();
    let child = child.ok_or(Status(st::NO_MORE_ENTRIES))?;
    let name = child.name.clone();
    write_key_info(c, &child, &name, class, buf, len, ret)
}

/// `NtQueryKey(handle, class, buffer, len, *ret_len)`.
pub fn nt_query_key(c: &mut Call<'_>) -> R {
    let (h, class, buf, len, ret) = (c.a32(0), c.a32(1), c.a(2), c.a32(3) as u64, c.a(4));
    let k = get_key(c, h)?;
    let name = k.name.clone();
    write_key_info(c, &k, &name, class, buf, len, ret)
}
