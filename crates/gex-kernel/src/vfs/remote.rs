//! A filesystem that lives in another wasm module.
//!
//! Each guest process is a Worker with its own memory, so a `TmpFs` in one is
//! invisible to the others; shared files instead live in the global kernel
//! module every Worker instantiates against one shared memory. This is the
//! client half: a `FileSystem`/`FileHandle` holding only an id and forwarding
//! every call, like [`crate::proc::Remote`].
//!
//! Addresses, not pointers, cross the boundary: every call passes an offset
//! into the caller's memory and the other side copies with `memory.copy` over
//! two memories, which is why the buffers here are ordinary `Vec<u8>`.
//! Structures cross as bytes in the layouts encoded below; both modules
//! compile these same functions.

use std::sync::Arc;

use crate::errno::Errno;
use crate::vfs::{DirEntry, FileHandle, FileSystem, FileType, Metadata, SysResult};

/// The calls a remote filesystem is reached by.
///
/// Every address is an offset into the *calling* process's memory, and every
/// `i32` return is a count or a negated errno. Plain function pointers so a
/// host can fill them in from wasm imports; see [`crate::proc::Remote`].
#[derive(Clone, Copy)]
pub struct Remote {
    /// Open, returning a handle id or a negated errno.
    pub open: extern "C" fn(path: u32, path_len: u32, flags: u64, mode: u32) -> i32,
    pub close: extern "C" fn(handle: i32),
    /// A second id for one open file, for a child to inherit. Returns the
    /// new id or a negated errno.
    pub dup: extern "C" fn(handle: i32) -> i32,
    pub read: extern "C" fn(handle: i32, offset: u64, at: u32, len: u32) -> i32,
    pub write: extern "C" fn(handle: i32, offset: u64, at: u32, len: u32) -> i32,
    /// Write this handle's metadata as [`META_BYTES`] bytes at `at`.
    pub meta: extern "C" fn(handle: i32, at: u32) -> i32,
    pub truncate: extern "C" fn(handle: i32, len: u64) -> i32,
    /// Write the directory listing at `at`, or `-ERANGE` if `cap` is short.
    pub read_dir: extern "C" fn(handle: i32, at: u32, cap: u32) -> i32,
    pub path_meta: extern "C" fn(path: u32, path_len: u32, at: u32) -> i32,
    pub read_link: extern "C" fn(path: u32, path_len: u32, at: u32, cap: u32) -> i32,
    pub unlink: extern "C" fn(path: u32, path_len: u32) -> i32,
    pub mkdir: extern "C" fn(path: u32, path_len: u32, mode: u32) -> i32,
    pub rmdir: extern "C" fn(path: u32, path_len: u32) -> i32,
    pub rename: extern "C" fn(from: u32, from_len: u32, to: u32, to_len: u32) -> i32,
    pub chmod: extern "C" fn(path: u32, path_len: u32, mode: u32) -> i32,
    pub symlink: extern "C" fn(target: u32, target_len: u32, path: u32, path_len: u32) -> i32,
    pub link: extern "C" fn(from: u32, from_len: u32, to: u32, to_len: u32) -> i32,
    /// Put a socket node at a path.
    ///
    /// Not read -- only stat'd: Xvfb binds `/tmp/.X11-unix/X0` and its startup
    /// script waits for that path to exist before launching a client.
    pub mksocket: extern "C" fn(path: u32, path_len: u32) -> i32,
}

/// Serialised [`Metadata`] size. Both modules compile this file so they agree,
/// but they are separate wasm artefacts -- a change here means rebuilding both.
pub const META_BYTES: usize = 36;

/// `Metadata` as bytes, in the one layout both modules read.
pub fn encode_meta(m: &Metadata) -> [u8; META_BYTES] {
    let mut out = [0u8; META_BYTES];
    out[0..4].copy_from_slice(&(file_type_code(m.file_type)).to_le_bytes());
    out[4..8].copy_from_slice(&m.mode.to_le_bytes());
    out[8..16].copy_from_slice(&m.size.to_le_bytes());
    out[16..24].copy_from_slice(&m.ino.to_le_bytes());
    out[24..32].copy_from_slice(&m.mtime.to_le_bytes());
    out[32..36].copy_from_slice(&m.nlink.to_le_bytes());
    out
}

/// The inverse of [`encode_meta`]. A code this does not know becomes a
/// regular file, which is the answer that misleads a guest least.
pub fn decode_meta(b: &[u8]) -> Metadata {
    let u32at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let u64at = |i: usize| {
        let mut a = [0u8; 8];
        a.copy_from_slice(&b[i..i + 8]);
        u64::from_le_bytes(a)
    };
    Metadata {
        file_type: file_type_of(u32at(0)),
        mode: u32at(4),
        size: u64at(8),
        ino: u64at(16),
        mtime: u64at(24) as i64,
        nlink: u32at(32).max(1),
    }
}

pub fn file_type_code(t: FileType) -> u32 {
    match t {
        FileType::Regular => 0,
        FileType::Directory => 1,
        FileType::Symlink => 2,
        FileType::CharDevice => 3,
        FileType::Fifo => 4,
        FileType::Socket => 5,
    }
}

pub fn file_type_of(code: u32) -> FileType {
    match code {
        1 => FileType::Directory,
        2 => FileType::Symlink,
        3 => FileType::CharDevice,
        4 => FileType::Fifo,
        5 => FileType::Socket,
        _ => FileType::Regular,
    }
}

/// A directory listing as bytes: per entry a 16-byte header (name length, type
/// code, inode) then the name. No count -- the reader stops at end of bytes.
pub fn encode_dir(entries: &[DirEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    for e in entries {
        out.extend_from_slice(&(e.name.len() as u32).to_le_bytes());
        out.extend_from_slice(&file_type_code(e.file_type).to_le_bytes());
        out.extend_from_slice(&e.ino.to_le_bytes());
        out.extend_from_slice(e.name.as_bytes());
    }
    out
}

/// The inverse of [`encode_dir`]. A truncated tail is dropped rather than
/// guessed at.
pub fn decode_dir(mut b: &[u8]) -> Vec<DirEntry> {
    let mut out = Vec::new();
    while b.len() >= 16 {
        let n = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
        let t = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
        let mut ino = [0u8; 8];
        ino.copy_from_slice(&b[8..16]);
        if b.len() < 16 + n {
            break;
        }
        out.push(DirEntry {
            name: String::from_utf8_lossy(&b[16..16 + n]).into_owned(),
            file_type: file_type_of(t),
            ino: u64::from_le_bytes(ino),
        });
        b = &b[16 + n..];
    }
    out
}

/// A count or a negated errno, as every call here reports it.
fn ok(v: i32) -> Result<i32, Errno> {
    if v < 0 {
        Err(Errno(-v))
    } else {
        Ok(v)
    }
}

/// Where a path is staged for the other side to copy out. The address must be
/// real in this process's memory and stay valid across the call.
fn at(b: &[u8]) -> (u32, u32) {
    (b.as_ptr() as u32, b.len() as u32)
}

/// A filesystem served by the global kernel.
pub struct RemoteFs {
    remote: Remote,
}

impl RemoteFs {
    pub fn new(remote: Remote) -> RemoteFs {
        RemoteFs { remote }
    }

    /// The calls this filesystem is reached by, for a caller that has an id
    /// rather than a path -- a child adopting a descriptor across a fork.
    pub fn remote(&self) -> Remote {
        self.remote
    }
}

impl std::fmt::Debug for RemoteFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RemoteFs")
    }
}

impl FileSystem for RemoteFs {
    fn open(&self, path: &str, flags: u64, mode: u32) -> Result<Arc<dyn FileHandle>, Errno> {
        let p = path.as_bytes();
        let (a, n) = at(p);
        let h = ok((self.remote.open)(a, n, flags, mode))?;
        Ok(Arc::new(RemoteHandle {
            id: h,
            remote: self.remote,
        }))
    }

    fn metadata(&self, path: &str) -> Result<Metadata, Errno> {
        let p = path.as_bytes();
        let (a, n) = at(p);
        let mut buf = [0u8; META_BYTES];
        ok((self.remote.path_meta)(a, n, buf.as_mut_ptr() as u32))?;
        Ok(decode_meta(&buf))
    }

    fn read_link(&self, path: &str) -> Result<String, Errno> {
        let p = path.as_bytes();
        let (a, n) = at(p);
        // 4 KiB is `PATH_MAX`; a target longer than that is not a target a
        // guest could have made through this kernel.
        let mut buf = [0u8; 4096];
        let got = ok((self.remote.read_link)(
            a,
            n,
            buf.as_mut_ptr() as u32,
            buf.len() as u32,
        ))? as usize;
        Ok(String::from_utf8_lossy(&buf[..got]).into_owned())
    }

    fn unlink(&self, path: &str) -> Result<(), Errno> {
        let (a, n) = at(path.as_bytes());
        ok((self.remote.unlink)(a, n)).map(|_| ())
    }

    fn mkdir(&self, path: &str, mode: u32) -> Result<(), Errno> {
        let (a, n) = at(path.as_bytes());
        ok((self.remote.mkdir)(a, n, mode)).map(|_| ())
    }

    fn rmdir(&self, path: &str) -> Result<(), Errno> {
        let (a, n) = at(path.as_bytes());
        ok((self.remote.rmdir)(a, n)).map(|_| ())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), Errno> {
        let (fa, fn_) = at(from.as_bytes());
        let (ta, tn) = at(to.as_bytes());
        ok((self.remote.rename)(fa, fn_, ta, tn)).map(|_| ())
    }

    fn chmod(&self, path: &str, mode: u32) -> Result<(), Errno> {
        let (a, n) = at(path.as_bytes());
        ok((self.remote.chmod)(a, n, mode)).map(|_| ())
    }

    fn link(&self, from: &str, to: &str) -> Result<(), Errno> {
        let (fa, fn_) = at(from.as_bytes());
        let (ta, tn) = at(to.as_bytes());
        ok((self.remote.link)(fa, fn_, ta, tn)).map(|_| ())
    }

    fn symlink(&self, target: &str, path: &str) -> Result<(), Errno> {
        let (ta, tn) = at(target.as_bytes());
        let (pa, pn) = at(path.as_bytes());
        ok((self.remote.symlink)(ta, tn, pa, pn)).map(|_| ())
    }

    fn mksocket(&self, path: &str) -> Result<(), Errno> {
        let (a, n) = at(path.as_bytes());
        ok((self.remote.mksocket)(a, n)).map(|_| ())
    }
}

/// One open file in the global kernel; the id is the whole of it. Dropping it
/// closes there, so a process that exits drops its handles with its file table.
struct RemoteHandle {
    id: i32,
    remote: Remote,
}

impl std::fmt::Debug for RemoteHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RemoteHandle({})", self.id)
    }
}

impl Drop for RemoteHandle {
    fn drop(&mut self) {
        (self.remote.close)(self.id);
    }
}

/// A handle onto an id the global kernel already holds -- what `fork` hands a
/// child, so an unlinked or renamed file crosses intact.
pub fn adopt(remote: Remote, id: i32) -> Arc<dyn FileHandle> {
    Arc::new(RemoteHandle { id, remote })
}

impl FileHandle for RemoteHandle {
    fn remote_id(&self) -> Option<i32> {
        Some(self.id)
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> SysResult {
        let n = ok((self.remote.read)(
            self.id,
            offset,
            buf.as_mut_ptr() as u32,
            buf.len() as u32,
        ))?;
        Ok(n as u64)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> SysResult {
        let n = ok((self.remote.write)(
            self.id,
            offset,
            buf.as_ptr() as u32,
            buf.len() as u32,
        ))?;
        Ok(n as u64)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        let mut buf = [0u8; META_BYTES];
        ok((self.remote.meta)(self.id, buf.as_mut_ptr() as u32))?;
        Ok(decode_meta(&buf))
    }

    fn read_dir(&self) -> Result<Vec<DirEntry>, Errno> {
        // Grown rather than sized up front: a listing has no bound the caller
        // knows, and asking its size first is a second call that can go stale.
        let mut cap = 4096;
        loop {
            let mut buf = vec![0u8; cap];
            match (self.remote.read_dir)(self.id, buf.as_mut_ptr() as u32, cap as u32) {
                n if n >= 0 => {
                    buf.truncate(n as usize);
                    return Ok(decode_dir(&buf));
                }
                n if -n == Errno::ERANGE.0 && cap < 1 << 24 => cap *= 4,
                n => return Err(Errno(-n)),
            }
        }
    }

    fn truncate(&self, len: u64) -> Result<(), Errno> {
        ok((self.remote.truncate)(self.id, len)).map(|_| ())
    }
}
