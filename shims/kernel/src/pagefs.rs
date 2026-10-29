//! Directories the page serves: a host filesystem (a desktop's own files,
//! IndexedDB, OPFS, the network) mounted into the tree every process shares.
//!
//! The page answers asynchronously, and a guest's call blocks the thread
//! that made it, so each call here is one request handed to the host
//! (`env.page_fs`), which waits in the calling Worker until the page has
//! answered (web/runtime/src/mount.js). Nothing else waits: another thread,
//! another process, a call elsewhere in the tree all go on.
//!
//! Mounted here rather than in each process's mount table so that every
//! process, those already running too, sees a mount at once. The root's
//! exports send a path under a mount point here ([`find`]); a walk through
//! the root stops at the mount point, and the mount's own paths are taken as
//! they are (a mount's symlinks are not followed on the way through it).
//!
//! A request, little-endian: `op`, `mount`, `handle`, `flags`, `mode`, `len`
//! (u32 each), `offset` (u64), then `path` and `path2`, each a u32 length and
//! its bytes, then the data. The answer is the call's result (a count, a
//! handle, or a negated errno) and its bytes, written where the call said.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use gex_kernel::errno::{Errno, SysResult};
use gex_kernel::fs::oflags;
use gex_kernel::vfs::{DirEntry, FileHandle, FileSystem, FileType, Metadata};

#[link(wasm_import_module = "env")]
extern "C" {
    /// Hand the request at `req` (`req_len` bytes, this module's memory) to
    /// the page and wait for its answer: the result, and up to `cap` bytes
    /// of it written at `out`.
    fn page_fs(req: u32, req_len: u32, out: u32, cap: u32) -> i32;
}

pub const STAT: u32 = 1;
pub const LSTAT: u32 = 2;
pub const READDIR: u32 = 3;
pub const OPEN: u32 = 4;
pub const READ: u32 = 5;
pub const WRITE: u32 = 6;
pub const CLOSE: u32 = 7;
pub const MKDIR: u32 = 8;
pub const UNLINK: u32 = 9;
pub const RMDIR: u32 = 10;
pub const RENAME: u32 = 11;
pub const TRUNCATE: u32 = 12;
pub const CHMOD: u32 = 13;
pub const UTIMES: u32 = 14;
pub const READLINK: u32 = 15;
pub const SYMLINK: u32 = 16;

/// The most one request carries or one answer brings back: a read or a write
/// is cut into calls of this much (the host's slot holds it).
pub const CHUNK: usize = 128 << 10;
/// A STAT's answer: type, mode, size, mtime, ino.
const STAT_BYTES: usize = 32;

/// Whether anything is mounted: the one load every root export pays.
static ANY: AtomicBool = AtomicBool::new(false);
/// Mount points, relative to the root as the root's exports name paths
/// (`root`, `home/user/files`), longest first.
static MOUNTS: RwLock<Vec<(String, Arc<PageFs>)>> = RwLock::new(Vec::new());

/// Mount the page's filesystem `id` at `at` (relative to the root).
pub fn mount(at: &str, id: u32) {
    let at = at.trim_matches('/').to_string();
    let mut m = MOUNTS.write().unwrap_or_else(|e| e.into_inner());
    m.retain(|(a, _)| *a != at);
    m.push((at, Arc::new(PageFs { id })));
    m.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    ANY.store(true, Ordering::Release);
}

/// Unmount what is at `at`; whether something was.
pub fn unmount(at: &str) -> bool {
    let at = at.trim_matches('/');
    let mut m = MOUNTS.write().unwrap_or_else(|e| e.into_inner());
    let n = m.len();
    m.retain(|(a, _)| a != at);
    ANY.store(!m.is_empty(), Ordering::Release);
    m.len() != n
}

#[inline]
pub fn any() -> bool {
    ANY.load(Ordering::Acquire)
}

fn under<'a>(p: &'a str, at: &str) -> Option<&'a str> {
    let rest = p.strip_prefix(at)?;
    if rest.is_empty() {
        Some("")
    } else {
        rest.strip_prefix('/')
    }
}

/// The mount holding `p` (relative to the root, or absolute) and the path in
/// it (relative, "" for the mount point itself).
pub fn find(p: &str) -> Option<(Arc<PageFs>, String)> {
    if !any() {
        return None;
    }
    let p = p.trim_start_matches('/');
    let m = MOUNTS.read().unwrap_or_else(|e| e.into_inner());
    m.iter().find_map(|(at, fs)| under(p, at).map(|rest| (fs.clone(), rest.to_string())))
}

/// The mount points, absolute and NUL-separated, after `others` (a walk's
/// `mounts`).
pub fn with_points(others: &str) -> String {
    let m = MOUNTS.read().unwrap_or_else(|e| e.into_inner());
    let mut s = others.to_string();
    for (at, _) in m.iter() {
        if !s.is_empty() {
            s.push('\0');
        }
        s.push('/');
        s.push_str(at);
    }
    s
}

struct Req {
    b: Vec<u8>,
}

impl Req {
    fn new(op: u32, id: u32) -> Req {
        let mut b = Vec::with_capacity(64);
        for v in [op, id, 0, 0, 0, 0] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&0u64.to_le_bytes());
        Req { b }
    }
    fn set(mut self, at: usize, v: u32) -> Req {
        self.b[at * 4..at * 4 + 4].copy_from_slice(&v.to_le_bytes());
        self
    }
    fn handle(self, h: u32) -> Req {
        self.set(2, h)
    }
    fn flags(self, f: u32) -> Req {
        self.set(3, f)
    }
    fn mode(self, m: u32) -> Req {
        self.set(4, m)
    }
    fn len(self, n: u32) -> Req {
        self.set(5, n)
    }
    fn offset(mut self, o: u64) -> Req {
        self.b[24..32].copy_from_slice(&o.to_le_bytes());
        self
    }
    fn paths(mut self, a: &str, b: &str) -> Req {
        for p in [a, b] {
            self.b.extend_from_slice(&(p.len() as u32).to_le_bytes());
            self.b.extend_from_slice(p.as_bytes());
        }
        self
    }
    fn data(mut self, d: &[u8]) -> Req {
        self.b.extend_from_slice(d);
        self
    }
    /// Send it; the result and the answer's bytes, `cap` at most.
    fn call(&self, out: &mut [u8]) -> Result<usize, Errno> {
        // SAFETY: both buffers are this module's memory, borrowed for the call.
        let r = unsafe { page_fs(self.b.as_ptr() as u32, self.b.len() as u32, out.as_mut_ptr() as u32, out.len() as u32) };
        if r < 0 {
            Err(Errno(-r))
        } else {
            Ok(r as usize)
        }
    }
    fn unit(&self) -> Result<(), Errno> {
        self.call(&mut []).map(|_| ())
    }
}

/// `/x/y`, as the page names a path in its filesystem.
fn abs(rel: &str) -> String {
    format!("/{}", rel.trim_matches('/'))
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn file_type(t: u32) -> FileType {
    match t {
        2 => FileType::Directory,
        3 => FileType::Symlink,
        _ => FileType::Regular,
    }
}

/// One mount: the page's filesystem `id`.
pub struct PageFs {
    id: u32,
}

impl PageFs {
    fn req(&self, op: u32) -> Req {
        Req::new(op, self.id)
    }

    fn stat(&self, op: u32, rel: &str) -> Result<Metadata, Errno> {
        let mut out = [0u8; STAT_BYTES];
        self.req(op).paths(&abs(rel), "").call(&mut out)?;
        Ok(self.meta(&out, rel))
    }

    fn meta(&self, b: &[u8], rel: &str) -> Metadata {
        let ft = file_type(u32_at(b, 0));
        let mut ino = u64_at(b, 24);
        if ino == 0 {
            // The page keeps none: one from the name, the same for the
            // file's life under it, apart from every other mount's.
            let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ ((self.id as u64) << 32);
            for c in rel.trim_matches('/').bytes() {
                h = (h ^ c as u64).wrapping_mul(0x100_0000_01b3);
            }
            ino = (h >> 2) | 2;
        }
        Metadata {
            file_type: ft,
            size: u64_at(b, 8),
            mode: u32_at(b, 4) & 0o7777,
            ino,
            mtime: u64_at(b, 16) as i64,
            nlink: if ft == FileType::Directory { 2 } else { 1 },
            rdev: 0,
            times: Default::default(),
        }
    }

    fn list(&self, rel: &str) -> Result<Vec<DirEntry>, Errno> {
        let path = abs(rel);
        let mut out = vec![0u8; CHUNK];
        let mut entries = Vec::new();
        // In pages: each answer is the entries from `offset` on that fit.
        loop {
            let n = self.req(READDIR).offset(entries.len() as u64).paths(&path, "").call(&mut out)?;
            if n == 0 {
                return Ok(entries);
            }
            let mut at = 0;
            for _ in 0..n {
                let t = out[at] as u32;
                let len = u16::from_le_bytes([out[at + 1], out[at + 2]]) as usize;
                let name = String::from_utf8_lossy(&out[at + 3..at + 3 + len]).into_owned();
                at += 3 + len;
                let child = format!("{}/{}", rel.trim_matches('/'), name);
                let ino = self.meta(&[0u8; STAT_BYTES], &child).ino;
                entries.push(DirEntry { name, file_type: file_type(t), ino });
            }
        }
    }
}

impl FileSystem for PageFs {
    fn fs_type(&self) -> &'static str {
        "page"
    }

    fn open(&self, rel: &str, flags: u64, mode: u32) -> Result<Arc<dyn FileHandle>, Errno> {
        let mut out = [0u8; STAT_BYTES + 4];
        // The page opens a file; a directory it only stats (handle -1).
        self.req(OPEN).flags(flags as u32).mode(mode).paths(&abs(rel), "").call(&mut out)?;
        let meta = self.meta(&out, rel);
        let h = i32::from_le_bytes(out[STAT_BYTES..].try_into().unwrap());
        if meta.file_type == FileType::Directory {
            if flags & oflags::ACCMODE != oflags::O_RDONLY {
                return Err(Errno::EISDIR);
            }
        } else if flags & oflags::O_DIRECTORY != 0 {
            if h >= 0 {
                let _ = self.req(CLOSE).handle(h as u32).unit();
            }
            return Err(Errno::ENOTDIR);
        }
        Ok(Arc::new(PageFile { fs: PageFs { id: self.id }, rel: rel.to_string(), h, meta: std::sync::Mutex::new(meta) }))
    }

    fn metadata(&self, rel: &str) -> Result<Metadata, Errno> {
        self.stat(STAT, rel)
    }

    fn metadata_nofollow(&self, rel: &str) -> Result<Metadata, Errno> {
        self.stat(LSTAT, rel)
    }

    fn read_link(&self, rel: &str) -> Result<String, Errno> {
        let mut out = vec![0u8; 4096];
        let n = self.req(READLINK).paths(&abs(rel), "").call(&mut out)?;
        Ok(String::from_utf8_lossy(&out[..n.min(out.len())]).into_owned())
    }

    fn unlink(&self, rel: &str) -> Result<(), Errno> {
        self.req(UNLINK).paths(&abs(rel), "").unit()
    }

    fn mkdir(&self, rel: &str, mode: u32) -> Result<(), Errno> {
        self.req(MKDIR).mode(mode).paths(&abs(rel), "").unit()
    }

    fn rmdir(&self, rel: &str) -> Result<(), Errno> {
        self.req(RMDIR).paths(&abs(rel), "").unit()
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), Errno> {
        self.req(RENAME).paths(&abs(from), &abs(to)).unit()
    }

    fn rename2(&self, from: &str, to: &str, noreplace: bool) -> Result<(), Errno> {
        if noreplace && self.stat(LSTAT, to).is_ok() {
            return Err(Errno::EEXIST);
        }
        self.rename(from, to)
    }

    fn chmod(&self, rel: &str, mode: u32) -> Result<(), Errno> {
        self.req(CHMOD).mode(mode).paths(&abs(rel), "").unit()
    }

    fn set_mtime(&self, rel: &str, secs: i64) -> Result<(), Errno> {
        self.req(UTIMES).offset(secs as u64).paths(&abs(rel), "").unit()
    }

    fn set_times(&self, rel: &str, _atime: Option<(i64, u32)>, mtime: Option<(i64, u32)>) -> Result<(), Errno> {
        match mtime {
            Some((s, _)) => self.set_mtime(rel, s),
            None => Ok(()),
        }
    }

    fn symlink(&self, target: &str, rel: &str) -> Result<(), Errno> {
        self.req(SYMLINK).paths(target, &abs(rel)).unit()
    }

    fn link(&self, _from: &str, _to: &str) -> Result<(), Errno> {
        Err(Errno::EPERM)
    }
}

/// An open file or directory of a mount. `h` is the page's handle, -1 for a
/// directory (listed by its path).
pub struct PageFile {
    fs: PageFs,
    rel: String,
    h: i32,
    /// What the open said, and the size since, as this handle wrote it.
    meta: std::sync::Mutex<Metadata>,
}

impl std::fmt::Debug for PageFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PageFile({}:{}, {})", self.fs.id, self.rel, self.h)
    }
}

impl Drop for PageFile {
    fn drop(&mut self) {
        if self.h >= 0 {
            let _ = self.fs.req(CLOSE).handle(self.h as u32).unit();
        }
    }
}

impl FileHandle for PageFile {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> SysResult {
        if self.h < 0 {
            return Err(Errno::EISDIR);
        }
        let mut done = 0;
        while done < buf.len() {
            let want = (buf.len() - done).min(CHUNK);
            let n = self
                .fs
                .req(READ)
                .handle(self.h as u32)
                .len(want as u32)
                .offset(offset + done as u64)
                .call(&mut buf[done..done + want])?;
            done += n.min(want);
            if n < want {
                break;
            }
        }
        Ok(done as u64)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> SysResult {
        if self.h < 0 {
            return Err(Errno::EISDIR);
        }
        let mut done = 0;
        while done < buf.len() {
            let part = &buf[done..(done + CHUNK).min(buf.len())];
            let n = self
                .fs
                .req(WRITE)
                .handle(self.h as u32)
                .len(part.len() as u32)
                .offset(offset + done as u64)
                .paths("", "")
                .data(part)
                .call(&mut [])?;
            done += n.min(part.len());
            if n < part.len() {
                break;
            }
        }
        let mut m = self.meta.lock().unwrap_or_else(|e| e.into_inner());
        m.size = m.size.max(offset + done as u64);
        Ok(done as u64)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        // The page's, when it answers (another writer may have changed the
        // file); this handle's own idea otherwise (unlinked since).
        match self.fs.stat(STAT, &self.rel) {
            Ok(m) => {
                *self.meta.lock().unwrap_or_else(|e| e.into_inner()) = m.clone();
                Ok(m)
            }
            Err(_) => Ok(self.meta.lock().unwrap_or_else(|e| e.into_inner()).clone()),
        }
    }

    fn read_dir(&self) -> Result<Vec<DirEntry>, Errno> {
        if self.h >= 0 {
            return Err(Errno::ENOTDIR);
        }
        self.fs.list(&self.rel)
    }

    fn truncate(&self, len: u64) -> Result<(), Errno> {
        let h = if self.h >= 0 { self.h as u32 } else { u32::MAX };
        self.fs.req(TRUNCATE).handle(h).offset(len).paths(&abs(&self.rel), "").unit()?;
        self.meta.lock().unwrap_or_else(|e| e.into_inner()).size = len;
        Ok(())
    }

    fn set_mtime(&self, secs: i64) -> Result<(), Errno> {
        self.fs.set_mtime(&self.rel, secs)
    }

    fn set_times(&self, atime: Option<(i64, u32)>, mtime: Option<(i64, u32)>) -> Result<(), Errno> {
        self.fs.set_times(&self.rel, atime, mtime)
    }

    fn chmod(&self, mode: u32) -> Option<Result<(), Errno>> {
        Some(self.fs.chmod(&self.rel, mode))
    }
}
