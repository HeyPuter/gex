//! The synthetic `/dev`.
//!
//! Only the character devices a userspace program actually opens: the null
//! and zero sinks, the random sources, the standard-stream symlinks and
//! `/dev/tty`. Everything else is absent rather than faked, so a program that
//! needs a real device fails where it would on a container.
//!
//! The host may add its own. That is how the guest gets a graphics device: a
//! node here whose `ioctl` reaches code outside the guest entirely.

use super::{DirEntry, FileHandle, FileSystem, FileType, Metadata};
use crate::errno::{Errno, SysResult};
use std::sync::Arc;

/// Which device a handle refers to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Device {
    Null,
    Zero,
    Full,
    Random,
    Tty,
}

/// The Linux device number (`makedev` encoding) each node reports as
/// `st_rdev`: /dev/null 1:3, zero 1:5, full 1:7, random 1:8, tty 5:0. Distinct
/// on purpose: a program may tell its streams apart by device -- Wine turns
/// all its debug output off when stderr's device is /dev/null's.
fn rdev_of(device: Device) -> u64 {
    match device {
        Device::Null => 0x103,
        Device::Zero => 0x105,
        Device::Full => 0x107,
        Device::Random => 0x108,
        Device::Tty => 0x500,
    }
}

#[derive(Debug)]
struct DevHandle {
    device: Device,
}

impl FileHandle for DevHandle {
    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> SysResult {
        match self.device {
            Device::Null => Ok(0),
            Device::Zero | Device::Full => {
                buf.fill(0);
                Ok(buf.len() as u64)
            }
            Device::Random => {
                fill_random(buf);
                Ok(buf.len() as u64)
            }
            // A terminal read has to come from the host's stdin, which the
            // kernel wires up separately; through the VFS it looks empty.
            Device::Tty => Ok(0),
        }
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> SysResult {
        match self.device {
            Device::Full => Err(Errno::ENOSPC),
            _ => Ok(buf.len() as u64),
        }
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata {
            file_type: FileType::CharDevice,
            size: 0,
            mode: 0o666,
            ino: 1,
            mtime: 0,
            nlink: 1,
            rdev: rdev_of(self.device),
            times: Default::default(),
        })
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn is_tty(&self) -> bool {
        self.device == Device::Tty
    }
}

/// The `/dev` filesystem.
pub struct DevFs {
    /// Nodes the host added, by name without a leading slash.
    extra: Vec<(String, Arc<dyn FileHandle>)>,
    /// Nodes the host added whose every open is a handle of its own (an
    /// audio device: each open is a stream with its own format and buffer).
    per_open: Vec<(String, Arc<dyn Fn() -> Arc<dyn FileHandle> + Send + Sync>)>,
    /// The pts numbers in use, when the system has pseudoterminals: `/dev/pts`
    /// lists them and `stat` answers for each (and for `/dev/ptmx`). Opening
    /// one is the kernel's (`open_pty`), not this filesystem's.
    pts: Option<Arc<dyn Fn() -> Vec<u32> + Send + Sync>>,
}

impl DevFs {
    pub fn new() -> DevFs {
        DevFs { extra: Vec::new(), per_open: Vec::new(), pts: None }
    }

    /// `/dev/ptmx` and `/dev/pts`, with `list` saying which pts numbers exist.
    pub fn add_pts(&mut self, list: Arc<dyn Fn() -> Vec<u32> + Send + Sync>) {
        self.pts = Some(list);
    }

    fn pts_node(&self, path: &str) -> Option<Metadata> {
        let list = self.pts.as_ref()?;
        let (ino, rdev, mode) = if path == "ptmx" {
            (crate::pty::PTMX_INO, crate::pty::PTMX_RDEV, 0o666)
        } else {
            let n: u32 = path.strip_prefix("pts/")?.parse().ok()?;
            if !list().contains(&n) {
                return None;
            }
            (crate::pty::pts_ino(n), crate::pty::pts_rdev(n), 0o620)
        };
        Some(Metadata {
            file_type: FileType::CharDevice,
            size: 0,
            mode,
            ino,
            mtime: 0,
            nlink: 1,
            rdev,
            times: Default::default(),
        })
    }

    /// Add a device the host provides. The handle is shared, not cloned per
    /// open, so a device keeps state (e.g. a graphics context) across opens.
    pub fn add(&mut self, name: &str, handle: Arc<dyn FileHandle>) {
        self.extra.retain(|(n, _)| n != name);
        self.extra.push((name.to_string(), handle));
    }

    /// Add a device whose every open gets a new handle from `open`.
    pub fn add_per_open(&mut self, name: &str, open: Arc<dyn Fn() -> Arc<dyn FileHandle> + Send + Sync>) {
        self.per_open.retain(|(n, _)| n != name);
        self.per_open.push((name.to_string(), open));
    }

    /// The OSS audio devices, `dsp` and `mixer` (`oss`: a null output).
    pub fn add_oss(&mut self) {
        use super::oss::{OssHandle, OssNode};
        self.add_per_open("dsp", Arc::new(|| Arc::new(OssHandle::new(OssNode::Dsp)) as Arc<dyn FileHandle>));
        self.add_per_open("mixer", Arc::new(|| Arc::new(OssHandle::new(OssNode::Mixer)) as Arc<dyn FileHandle>));
    }

    fn device_of(path: &str) -> Option<Device> {
        Some(match path {
            "null" => Device::Null,
            "zero" => Device::Zero,
            "full" => Device::Full,
            "random" | "urandom" => Device::Random,
            "tty" | "console" => Device::Tty,
            _ => return None,
        })
    }
}

impl Default for DevFs {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSystem for DevFs {
    fn fs_type(&self) -> &'static str {
        "devtmpfs"
    }
    fn open(&self, path: &str, flags: u64, _mode: u32) -> Result<Arc<dyn FileHandle>, Errno> {
        // A directory opens for reading only.
        if super::writes(flags) && self.metadata(path).is_ok_and(|md| md.file_type == FileType::Directory) {
            return Err(Errno::EISDIR);
        }
        if let Some(device) = Self::device_of(path) {
            return Ok(Arc::new(DevHandle { device }));
        }
        if let Some((_, handle)) = self.extra.iter().find(|(n, _)| n == path) {
            return Ok(Arc::clone(handle));
        }
        if let Some((_, open)) = self.per_open.iter().find(|(n, _)| n == path) {
            return Ok(open());
        }
        if path.is_empty() {
            return Ok(Arc::new(DevDir {
                extra: self.extra.iter().map(|(n, _)| n.clone()).chain(self.per_open.iter().map(|(n, _)| n.clone())).collect(),
                pts: self.pts.is_some(),
            }));
        }
        if let (Some(list), "pts") = (self.pts.as_ref(), path) {
            return Ok(Arc::new(PtsDir { numbers: list() }));
        }
        Err(Errno::ENOENT)
    }

    fn metadata(&self, path: &str) -> Result<Metadata, Errno> {
        if path.is_empty() {
            return Ok(Metadata::dir());
        }
        if let Some((_, open)) = self.per_open.iter().find(|(n, _)| n == path) {
            return open().metadata();
        }
        if let Some(device) = Self::device_of(path) {
            return Ok(Metadata {
                file_type: FileType::CharDevice,
                size: 0,
                mode: 0o666,
                ino: 1,
                mtime: 0,
                nlink: 1,
                rdev: rdev_of(device),
                times: Default::default(),
            });
        }
        // A device the host added (`add`): what its handle says, as `fstat`
        // of an open one does. Without this `open` found it and `stat` did
        // not, so `ls /dev/gex-gl` and `access` said there was no GPU.
        if let Some((_, handle)) = self.extra.iter().find(|(n, _)| n == path) {
            return handle.metadata();
        }
        if path == "pts" && self.pts.is_some() {
            return Ok(Metadata::dir());
        }
        if let Some(md) = self.pts_node(path) {
            return Ok(md);
        }
        if matches!(path, "stdin" | "stdout" | "stderr" | "fd") {
            return Ok(Metadata {
                file_type: FileType::Symlink,
                size: 0,
                mode: 0o777,
                ino: 1,
                mtime: 0,
                nlink: 1,
                rdev: 0,
                times: Default::default(),
            });
        }
        Err(Errno::ENOENT)
    }

    fn read_link(&self, path: &str) -> Result<String, Errno> {
        Ok(match path {
            "stdin" => "/proc/self/fd/0".to_string(),
            "stdout" => "/proc/self/fd/1".to_string(),
            "stderr" => "/proc/self/fd/2".to_string(),
            "fd" => "/proc/self/fd".to_string(),
            _ => {
                self.metadata(path)?;
                return Err(Errno::EINVAL);
            }
        })
    }
}

#[derive(Debug)]
struct DevDir {
    /// Names the host added, so `ls /dev` shows them.
    extra: Vec<String>,
    /// Whether `ptmx` and `pts` exist.
    pts: bool,
}

/// `/dev/pts`: one character device per pseudoterminal in use.
#[derive(Debug)]
struct PtsDir {
    numbers: Vec<u32>,
}

impl FileHandle for PtsDir {
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> SysResult {
        Err(Errno::EISDIR)
    }
    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata::dir())
    }
    fn read_dir(&self) -> Result<Vec<DirEntry>, Errno> {
        let mut out = vec![
            DirEntry { name: ".".into(), file_type: FileType::Directory, ino: 1 },
            DirEntry { name: "..".into(), file_type: FileType::Directory, ino: 1 },
        ];
        for &n in &self.numbers {
            out.push(DirEntry { name: n.to_string(), file_type: FileType::CharDevice, ino: crate::pty::pts_ino(n) });
        }
        Ok(out)
    }
}

impl FileHandle for DevDir {
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> SysResult {
        Err(Errno::EISDIR)
    }
    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata::dir())
    }
    fn read_dir(&self) -> Result<Vec<DirEntry>, Errno> {
        let mut out = vec![
            DirEntry {
                name: ".".into(),
                file_type: FileType::Directory,
                ino: 1,
            },
            DirEntry {
                name: "..".into(),
                file_type: FileType::Directory,
                ino: 1,
            },
        ];
        for name in [
            "null", "zero", "full", "random", "urandom", "tty", "console",
        ]
        .iter()
        .map(|s| s.to_string())
        .chain(self.extra.iter().cloned())
        {
            out.push(DirEntry {
                name: name.into(),
                file_type: FileType::CharDevice,
                ino: 1,
            });
        }
        if self.pts {
            out.push(DirEntry { name: "ptmx".into(), file_type: FileType::CharDevice, ino: crate::pty::PTMX_INO });
            out.push(DirEntry { name: "pts".into(), file_type: FileType::Directory, ino: 1 });
        }
        for name in ["stdin", "stdout", "stderr", "fd"] {
            out.push(DirEntry {
                name: name.into(),
                file_type: FileType::Symlink,
                ino: 1,
            });
        }
        Ok(out)
    }
}

/// Pseudorandom bytes.
///
/// Deterministic by default so record/replay stays possible; a
/// host that wants real entropy replaces the `/dev/random` backend.
/// `/dev/urandom` and `getrandom`'s bytes: splitmix64 over a per-thread
/// state. The state was a constant xorshift seed in every thread, so every
/// thread (each guest thread is a host thread, a Worker in the page) read
/// the same sequence: two of services.exe's RPC threads made the same
/// context-handle UUIDs, a client got another service's handle back and a
/// driver's start never reported (a 120 s stall in the Windows shell's
/// boot), and every process's TLS keys came from one sequence. Now each
/// thread's state starts from the clock, the thread's id and a counter
/// shared by the threads of this kernel, and every read mixes that counter
/// in again. Not a cryptographic generator: the host's entropy is not
/// reachable from here.
fn fill_random(buf: &mut [u8]) {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicU64, Ordering};
    const GOLDEN: u64 = 0x9e37_79b9_7f4a_7c15;
    static SHARED: AtomicU64 = AtomicU64::new(0);
    fn mix(mut z: u64) -> u64 {
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0) };
    }
    let n = SHARED.fetch_add(1, Ordering::Relaxed);
    STATE.with(|s| {
        let mut x = s.get();
        if x == 0 {
            let local = 0u8;
            x = mix(super::tmpfs::now_ns() ^ mix(crate::sync::me() as u64 ^ (n << 32))
                ^ mix(&local as *const u8 as u64)) | 1;
        }
        x ^= mix(n.wrapping_mul(GOLDEN));
        for chunk in buf.chunks_mut(8) {
            x = x.wrapping_add(GOLDEN);
            let bytes = mix(x).to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
        s.set(x);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_differs_between_threads() {
        let read = || {
            let mut b = [0u8; 32];
            fill_random(&mut b);
            b
        };
        let a = read();
        let b = std::thread::spawn(read).join().unwrap();
        let c = std::thread::spawn(read).join().unwrap();
        assert_ne!(a, b, "two threads read the same bytes");
        assert_ne!(b, c, "two threads read the same bytes");
        assert_ne!(read(), a, "one thread read the same bytes twice");
    }

    #[test]
    fn device_numbers_are_real_and_distinct() {
        let fs = DevFs::new();
        let null = fs.metadata("null").unwrap();
        assert_eq!(null.rdev, 0x103, "/dev/null is 1:3");
        assert_ne!(null.rdev, fs.metadata("tty").unwrap().rdev);
        assert_ne!(null.rdev, fs.metadata("zero").unwrap().rdev);
        // The same number through an open descriptor, which is what `fstat` sees.
        assert_eq!(fs.open("null", 0, 0).unwrap().metadata().unwrap().rdev, 0x103);
    }

    #[test]
    fn null_reads_empty_and_swallows_writes() {
        let f = DevFs::new().open("null", 0, 0).unwrap();
        let mut buf = [0xffu8; 8];
        assert_eq!(f.read_at(0, &mut buf).unwrap(), 0);
        assert_eq!(f.write_at(0, b"discarded").unwrap(), 9);
    }

    #[test]
    fn zero_reads_zeros() {
        let f = DevFs::new().open("zero", 0, 0).unwrap();
        let mut buf = [0xffu8; 8];
        assert_eq!(f.read_at(0, &mut buf).unwrap(), 8);
        assert_eq!(buf, [0u8; 8]);
    }

    #[test]
    fn full_fails_writes_with_enospc() {
        let f = DevFs::new().open("full", 0, 0).unwrap();
        assert_eq!(f.write_at(0, b"x").err(), Some(Errno::ENOSPC));
    }

    #[test]
    fn standard_stream_names_are_symlinks_into_proc() {
        assert_eq!(DevFs::new().read_link("stdout").unwrap(), "/proc/self/fd/1");
        assert_eq!(
            DevFs::new().metadata("stdout").unwrap().file_type,
            FileType::Symlink
        );
    }

    #[test]
    fn unknown_devices_are_absent_rather_than_faked() {
        assert_eq!(DevFs::new().open("sda1", 0, 0).err(), Some(Errno::ENOENT));
    }

    /// A device the host adds is opened like any other, and the same handle
    /// comes back every time -- which is what lets one keep state across the
    /// several times a program opens it.
    /// `ttyname` compares `fstat` of the descriptor with `stat` of the name
    /// `/proc/self/fd` gives: `/dev/pts/N` must exist, as the same node.
    #[test]
    fn pts_lists_the_pairs_in_use_with_their_device_numbers() {
        let mut fs = DevFs::new();
        assert_eq!(fs.metadata("pts").err(), Some(Errno::ENOENT), "no pseudoterminals, no /dev/pts");
        fs.add_pts(Arc::new(|| vec![0, 3]));
        assert_eq!(fs.metadata("pts").unwrap().file_type, FileType::Directory);
        let names: Vec<String> = fs.open("pts", 0, 0).unwrap().read_dir().unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, [".", "..", "0", "3"]);
        let md = fs.metadata("pts/3").unwrap();
        assert_eq!((md.file_type, md.rdev, md.ino), (FileType::CharDevice, 0x8803, crate::pty::pts_ino(3)));
        assert_eq!(fs.metadata("pts/1").err(), Some(Errno::ENOENT));
        assert_eq!(fs.metadata("ptmx").unwrap().rdev, (5 << 8) | 2);
        let root: Vec<String> = fs.open("", 0, 0).unwrap().read_dir().unwrap().into_iter().map(|e| e.name).collect();
        assert!(root.iter().any(|n| n == "pts") && root.iter().any(|n| n == "ptmx"));
    }

    #[test]
    fn a_host_device_is_openable_and_shared() {
        #[derive(Debug)]
        struct Counter(std::sync::atomic::AtomicU32);
        impl FileHandle for Counter {
            fn read_at(&self, _o: u64, _b: &mut [u8]) -> SysResult {
                Err(Errno::EINVAL)
            }
            fn metadata(&self) -> Result<Metadata, Errno> {
                Ok(Metadata {
                    file_type: FileType::CharDevice,
                    size: 0,
                    mode: 0o666,
                    ino: 0,
                    mtime: 0,
                    nlink: 1,
                    rdev: 0,
                    times: Default::default(),
                })
            }
            fn ioctl(
                &self,
                _m: &mut crate::GuestMem<'_>,
                request: u64,
                _arg: u64,
            ) -> Option<SysResult> {
                if request == 42 {
                    let n = self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Some(Ok(n as u64))
                } else {
                    None
                }
            }
        }
        let device = Arc::new(Counter(std::sync::atomic::AtomicU32::new(0)));
        let mut fs = DevFs::new();
        fs.add("gex-thing", device);
        let a = fs.open("gex-thing", 0, 0).unwrap();
        let b = fs.open("gex-thing", 0, 0).unwrap();
        let mut bytes = [0u8; 16];
        let mut mem = crate::GuestMem::new(&mut bytes);
        assert_eq!(a.ioctl(&mut mem, 42, 0), Some(Ok(0)));
        assert_eq!(b.ioctl(&mut mem, 42, 0), Some(Ok(1)), "the same device");
        assert_eq!(a.ioctl(&mut mem, 7, 0), None, "not its ioctl");
        // And it is listed, so `ls /dev` shows it.
        let names: Vec<String> = fs
            .open("", 0, 0)
            .unwrap()
            .read_dir()
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(names.iter().any(|n| n == "gex-thing"));
        // And stat finds what open does: the name, as the handle describes it.
        assert_eq!(fs.metadata("gex-thing").unwrap().file_type, FileType::CharDevice);
        assert_eq!(fs.metadata("gex-other").err(), Some(Errno::ENOENT));
    }
}
