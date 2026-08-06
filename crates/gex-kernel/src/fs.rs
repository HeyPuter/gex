//! Open-file constants and the `struct stat` layout.
//!
//! The descriptor table lives in [`crate::file`] and the backends in
//! [`crate::vfs`]; what stays here is the ABI vocabulary they share with the
//! guest.

/// Open-file flags the guest passes to `openat`.
pub mod oflags {
    pub const O_RDONLY: u64 = 0o0;
    pub const O_WRONLY: u64 = 0o1;
    pub const O_RDWR: u64 = 0o2;
    pub const O_CREAT: u64 = 0o100;
    pub const O_EXCL: u64 = 0o200;
    pub const O_TRUNC: u64 = 0o1000;
    pub const O_APPEND: u64 = 0o2000;
    pub const O_NONBLOCK: u64 = 0o4000;
    pub const O_DIRECTORY: u64 = 0o200000;
    pub const O_CLOEXEC: u64 = 0o2000000;
    /// An unnamed file created inside the named *directory*. The bit overlaps
    /// `O_DIRECTORY`, which is why it is always tested together with it.
    pub const O_TMPFILE: u64 = 0o20000000;
    /// A handle to a name rather than to its contents. Legal on a dangling
    /// symlink, and usable only for the `*at` calls and `fstat`.
    pub const O_PATH: u64 = 0o10000000;
    pub const O_NOFOLLOW: u64 = 0o400000;
    pub const ACCMODE: u64 = 0o3;
}

/// `st_mode` type bits.
pub mod mode {
    pub const S_IFMT: u32 = 0o170000;
    pub const S_IFIFO: u32 = 0o010000;
    pub const S_IFCHR: u32 = 0o020000;
    pub const S_IFDIR: u32 = 0o040000;
    pub const S_IFREG: u32 = 0o100000;
}

/// Metadata in the shape `fstat` reports.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stat {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub size: i64,
    pub blksize: i32,
    pub blocks: i64,
    pub mtime: i64,
}

impl Stat {
    /// Serialise into the aarch64 kernel `struct stat` (128 bytes).
    pub fn encode(&self) -> [u8; 128] {
        let mut b = [0u8; 128];
        b[0..8].copy_from_slice(&self.dev.to_le_bytes());
        b[8..16].copy_from_slice(&self.ino.to_le_bytes());
        b[16..20].copy_from_slice(&self.mode.to_le_bytes());
        b[20..24].copy_from_slice(&self.nlink.to_le_bytes());
        b[24..28].copy_from_slice(&self.uid.to_le_bytes());
        b[28..32].copy_from_slice(&self.gid.to_le_bytes());
        b[32..40].copy_from_slice(&self.rdev.to_le_bytes());
        b[48..56].copy_from_slice(&self.size.to_le_bytes());
        b[56..60].copy_from_slice(&self.blksize.to_le_bytes());
        b[64..72].copy_from_slice(&self.blocks.to_le_bytes());
        for off in [72usize, 88, 104] {
            b[off..off + 8].copy_from_slice(&self.mtime.to_le_bytes());
        }
        b
    }
}
