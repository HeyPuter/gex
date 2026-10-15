//! Open-file constants and the `struct stat` layout.
//!
//! The descriptor table lives in [`crate::file`] and the backends in
//! [`crate::vfs`]; what stays here is the ABI vocabulary they share with the
//! guest.

/// Open-file flags, in the kernel's own spelling: x86's, which i386 and
/// wasm32 programs use on every build. The four bits Linux spells
/// differently per architecture (`O_DIRECTORY`, `O_NOFOLLOW`, `O_DIRECT`,
/// `O_LARGEFILE`) arrive from an AArch64 program in the asm-generic spelling
/// and are turned into this one at the guest's door ([`from_guest`]); a ppc
/// program's go through `ppc::open_flags`.
pub mod oflags {
    pub const O_RDONLY: u64 = 0o0;
    pub const O_WRONLY: u64 = 0o1;
    pub const O_RDWR: u64 = 0o2;
    pub const O_CREAT: u64 = 0o100;
    pub const O_EXCL: u64 = 0o200;
    pub const O_TRUNC: u64 = 0o1000;
    pub const O_APPEND: u64 = 0o2000;
    pub const O_NONBLOCK: u64 = 0o4000;
    /// Opening a terminal does *not* make it this session's controlling one.
    /// Without this flag a session leader's open claims it -- see `open_pty`.
    pub const O_NOCTTY: u64 = 0o400;
    pub const O_DIRECTORY: u64 = 0o200000;
    pub const O_CLOEXEC: u64 = 0o2000000;
    /// An unnamed file created inside the named *directory*. The bit overlaps
    /// `O_DIRECTORY`, which is why it is always tested together with it.
    pub const O_TMPFILE: u64 = 0o20000000;
    /// A handle to a name rather than to its contents. Legal on a dangling
    /// symlink, and usable only for the `*at` calls and `fstat`.
    pub const O_PATH: u64 = 0o10000000;
    pub const O_NOFOLLOW: u64 = 0o400000;
    pub const O_DIRECT: u64 = 0o40000;
    /// i386's; x86-64 has none of its own (Linux forces it on there).
    pub const O_LARGEFILE: u64 = 0o100000;
    pub const ACCMODE: u64 = 0o3;

    /// The asm-generic spelling (AArch64's, and ppc's but for `O_DIRECT` and
    /// `O_LARGEFILE`, which ppc swaps).
    pub mod generic {
        pub const O_DIRECTORY: u64 = 0o40000;
        pub const O_NOFOLLOW: u64 = 0o100000;
        pub const O_DIRECT: u64 = 0o200000;
        pub const O_LARGEFILE: u64 = 0o400000;
    }

    /// Each pair (generic, kernel) of the bits that differ.
    const DIFFER: [(u64, u64); 4] = [
        (generic::O_DIRECTORY, O_DIRECTORY),
        (generic::O_NOFOLLOW, O_NOFOLLOW),
        (generic::O_DIRECT, O_DIRECT),
        (generic::O_LARGEFILE, O_LARGEFILE),
    ];

    /// Generic spelling to the kernel's.
    pub fn from_generic(f: u64) -> u64 {
        DIFFER.iter().fold(f & !DIFFER.iter().fold(0, |m, d| m | d.0), |out, &(g, k)| {
            if f & g != 0 { out | k } else { out }
        })
    }

    /// The kernel's spelling to generic (`fcntl(F_GETFL)`).
    pub fn to_generic(f: u64) -> u64 {
        DIFFER.iter().fold(f & !DIFFER.iter().fold(0, |m, d| m | d.1), |out, &(g, k)| {
            if f & k != 0 { out | g } else { out }
        })
    }

    /// Whether this process spells its open flags the generic way: an
    /// AArch64 program. Not an i386 or wasm32 one (`compat`, which use x86's
    /// on every build), and not a ppc one, which its own layer translates.
    pub fn generic_guest(compat: bool) -> bool {
        !gex_abi::X86 && !gex_abi::PPC && !compat
    }

    /// Flags as this process passed them, in the kernel's spelling.
    pub fn from_guest(f: u64, compat: bool) -> u64 {
        if generic_guest(compat) { from_generic(f) } else { f }
    }

    /// Flags in the kernel's spelling, as this process reads them.
    pub fn to_guest(f: u64, compat: bool) -> u64 {
        if generic_guest(compat) { to_generic(f) } else { f }
    }
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
    pub mtime_ns: u32,
    pub atime: i64,
    pub atime_ns: u32,
    pub ctime: i64,
    pub ctime_ns: u32,
}

impl Stat {
    /// (seconds, nanoseconds) of atime, mtime, ctime, in `struct stat`'s
    /// order.
    pub fn times(&self) -> [(i64, u32); 3] {
        [(self.atime, self.atime_ns), (self.mtime, self.mtime_ns), (self.ctime, self.ctime_ns)]
    }

    /// PowerPC's 32-bit `struct stat64` (104 bytes, big-endian): no padding
    /// after `st_dev`, a 64-bit `st_ino` second, 32-bit times.
    fn encode_ppc(&self) -> Vec<u8> {
        use gex_abi::GuestEndian;
        let mut b = vec![0u8; 104];
        b[0..8].copy_from_slice(&self.dev.to_ge_bytes());
        b[8..16].copy_from_slice(&self.ino.to_ge_bytes());
        b[16..20].copy_from_slice(&self.mode.to_ge_bytes());
        b[20..24].copy_from_slice(&self.nlink.to_ge_bytes());
        b[24..28].copy_from_slice(&self.uid.to_ge_bytes());
        b[28..32].copy_from_slice(&self.gid.to_ge_bytes());
        b[32..40].copy_from_slice(&self.rdev.to_ge_bytes());
        b[48..56].copy_from_slice(&self.size.to_ge_bytes());
        b[56..60].copy_from_slice(&self.blksize.to_ge_bytes());
        b[64..72].copy_from_slice(&self.blocks.to_ge_bytes());
        for (off, (t, ns)) in [72usize, 80, 88].into_iter().zip(self.times()) {
            b[off..off + 4].copy_from_slice(&(t as u32).to_ge_bytes());
            b[off + 4..off + 8].copy_from_slice(&ns.to_ge_bytes());
        }
        b
    }

    /// Serialise into the guest kernel's `struct stat`: 128 bytes on aarch64,
    /// 144 on x86-64, where `st_nlink` widens to 64 bits and precedes
    /// `st_mode` and `st_blksize` is 64 bits.
    pub fn encode(&self) -> Vec<u8> {
        self.encode_for(false)
    }

    /// As [`Stat::encode`], or the i386 `struct stat64` (96 bytes, packed to
    /// 4-byte alignment) for a 32-bit process.
    pub fn encode_for(&self, compat: bool) -> Vec<u8> {
        if gex_abi::PPC {
            return self.encode_ppc();
        }
        if compat {
            let mut b = vec![0u8; 96];
            b[0..8].copy_from_slice(&self.dev.to_le_bytes());
            b[12..16].copy_from_slice(&(self.ino as u32).to_le_bytes());
            b[16..20].copy_from_slice(&self.mode.to_le_bytes());
            b[20..24].copy_from_slice(&self.nlink.to_le_bytes());
            b[24..28].copy_from_slice(&self.uid.to_le_bytes());
            b[28..32].copy_from_slice(&self.gid.to_le_bytes());
            b[32..40].copy_from_slice(&self.rdev.to_le_bytes());
            b[44..52].copy_from_slice(&self.size.to_le_bytes());
            b[52..56].copy_from_slice(&(self.blksize as u32).to_le_bytes());
            b[56..64].copy_from_slice(&(self.blocks as u64).to_le_bytes());
            for (off, (t, ns)) in [64usize, 72, 80].into_iter().zip(self.times()) {
                b[off..off + 4].copy_from_slice(&(t as u32).to_le_bytes());
                b[off + 4..off + 8].copy_from_slice(&ns.to_le_bytes());
            }
            b[88..96].copy_from_slice(&self.ino.to_le_bytes());
            return b;
        }
        if gex_abi::X86 {
            let mut b = vec![0u8; 144];
            b[0..8].copy_from_slice(&self.dev.to_le_bytes());
            b[8..16].copy_from_slice(&self.ino.to_le_bytes());
            b[16..24].copy_from_slice(&(self.nlink as u64).to_le_bytes());
            b[24..28].copy_from_slice(&self.mode.to_le_bytes());
            b[28..32].copy_from_slice(&self.uid.to_le_bytes());
            b[32..36].copy_from_slice(&self.gid.to_le_bytes());
            b[40..48].copy_from_slice(&self.rdev.to_le_bytes());
            b[48..56].copy_from_slice(&self.size.to_le_bytes());
            b[56..64].copy_from_slice(&(self.blksize as i64).to_le_bytes());
            b[64..72].copy_from_slice(&self.blocks.to_le_bytes());
            for (off, (t, ns)) in [72usize, 88, 104].into_iter().zip(self.times()) {
                b[off..off + 8].copy_from_slice(&t.to_le_bytes());
                b[off + 8..off + 16].copy_from_slice(&(ns as u64).to_le_bytes());
            }
            return b;
        }
        let mut b = vec![0u8; 128];
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
        for (off, (t, ns)) in [72usize, 88, 104].into_iter().zip(self.times()) {
            b[off..off + 8].copy_from_slice(&t.to_le_bytes());
            b[off + 8..off + 16].copy_from_slice(&(ns as u64).to_le_bytes());
        }
        b
    }
}

#[cfg(test)]
mod oflag_tests {
    use super::oflags::*;

    #[test]
    fn generic_spelling_round_trips() {
        let all = O_RDWR | O_CREAT | O_CLOEXEC | O_NONBLOCK;
        for (g, k) in [
            (generic::O_DIRECTORY, O_DIRECTORY),
            (generic::O_NOFOLLOW, O_NOFOLLOW),
            (generic::O_DIRECT, O_DIRECT),
            (generic::O_LARGEFILE, O_LARGEFILE),
        ] {
            assert_eq!(from_generic(g | all), k | all, "{g:#o}");
            assert_eq!(to_generic(k | all), g | all, "{k:#o}");
        }
        let every = generic::O_DIRECTORY | generic::O_NOFOLLOW | generic::O_DIRECT | generic::O_LARGEFILE;
        assert_eq!(to_generic(from_generic(every | all)), every | all);
        // An i386 or wasm32 program spells them x86's way on every build.
        assert_eq!(from_guest(O_NOFOLLOW, true), O_NOFOLLOW);
    }
}
