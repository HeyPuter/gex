//! The synthetic `/proc`.
//!
//! Contents are generated on open from live kernel state rather than stored,
//! because that is what makes them true. `self/maps` in particular has to
//! reflect the VMA tree exactly: glibc, OpenSSL and every JVM read it, and
//! C++ exception unwinding locates `.eh_frame` through the same picture of
//! the address space (PLAN §6.4, §8).

use super::{DirEntry, FileHandle, FileSystem, FileType, Metadata};
use crate::errno::{Errno, SysResult};
use std::sync::Arc;

/// Live process state that `/proc` reports on.
///
/// The kernel implements this; keeping it a trait means procfs does not need
/// a reference back into the whole kernel, which would be a borrow knot.
pub trait ProcSource: Send + Sync {
    fn pid(&self) -> i32;
    fn maps(&self) -> String;
    fn cmdline(&self) -> Vec<u8>;
    fn environ(&self) -> Vec<u8>;
    fn exe(&self) -> String;
    fn cwd(&self) -> String;
    /// Names of the currently open descriptors, as `(fd, target)`.
    fn fds(&self) -> Vec<(i32, String)>;
    fn stat(&self) -> String;
    fn status(&self) -> String;
}

/// A generated file: contents are captured when it is opened.
#[derive(Debug)]
struct Generated {
    data: Vec<u8>,
}

impl FileHandle for Generated {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> SysResult {
        let start = (offset as usize).min(self.data.len());
        let n = buf.len().min(self.data.len() - start);
        buf[..n].copy_from_slice(&self.data[start..start + n]);
        Ok(n as u64)
    }
    fn metadata(&self) -> Result<Metadata, Errno> {
        // Linux reports size 0 for most of /proc; programs are expected to
        // read until EOF rather than trust it, and some break if a stale
        // size disagrees with the contents.
        Ok(Metadata {
            file_type: FileType::Regular,
            size: 0,
            mode: 0o444,
            ino: 1,
            mtime: 0,
            nlink: 1,
        })
    }
}

#[derive(Debug)]
struct GenDir {
    entries: Vec<DirEntry>,
}

impl FileHandle for GenDir {
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> SysResult {
        Err(Errno::EISDIR)
    }
    fn metadata(&self) -> Result<Metadata, Errno> {
        Ok(Metadata::dir())
    }
    fn read_dir(&self) -> Result<Vec<DirEntry>, Errno> {
        Ok(self.entries.clone())
    }
}

/// The `/proc` filesystem.
pub struct ProcFs {
    source: Arc<dyn ProcSource>,
}

impl ProcFs {
    pub fn new(source: Arc<dyn ProcSource>) -> ProcFs {
        ProcFs { source }
    }

    /// Rewrite a `<pid>/…` path to `self/…` so both spellings share one
    /// implementation, as they do on Linux for the current process.
    fn canonical(&self, path: &str) -> String {
        let pid = self.source.pid().to_string();
        match path.strip_prefix(&pid) {
            Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                format!("self{rest}")
            }
            _ => path.to_string(),
        }
    }

    fn generate(&self, path: &str) -> Option<Vec<u8>> {
        let s = &self.source;
        Some(match path {
            "self/maps" => s.maps().into_bytes(),
            "self/cmdline" => s.cmdline(),
            "self/environ" => s.environ(),
            "self/stat" => s.stat().into_bytes(),
            "self/status" => s.status().into_bytes(),
            "self/comm" => {
                let exe = s.exe();
                let name = exe.rsplit('/').next().unwrap_or("gex").to_string();
                format!("{name}\n").into_bytes()
            }
            "meminfo" => concat!(
                "MemTotal:        4194304 kB\n",
                "MemFree:         2097152 kB\n",
                "MemAvailable:    3145728 kB\n",
                "Buffers:               0 kB\n",
                "Cached:                0 kB\n",
                "SwapTotal:             0 kB\n",
                "SwapFree:              0 kB\n",
            )
            .as_bytes()
            .to_vec(),
            "cpuinfo" => concat!(
                "processor\t: 0\n",
                "BogoMIPS\t: 100.00\n",
                "Features\t: fp asimd atomics cpuid\n",
                "CPU implementer\t: 0x41\n",
                "CPU architecture: 8\n",
                "CPU variant\t: 0x0\n",
                "CPU part\t: 0xd08\n",
                "CPU revision\t: 3\n\n",
            )
            .as_bytes()
            .to_vec(),
            "version" => b"Linux version 6.1.0-gex (gex@puter) #1 SMP\n".to_vec(),
            "uptime" => b"1000.00 1000.00\n".to_vec(),
            "filesystems" => b"nodev\ttmpfs\nnodev\tproc\nnodev\tdevtmpfs\n\text4\n".to_vec(),
            "sys/kernel/osrelease" => b"6.1.0-gex\n".to_vec(),
            "sys/kernel/pid_max" => b"32768\n".to_vec(),
            // glibc and musl both consult this before using large mappings.
            "sys/vm/overcommit_memory" => b"0\n".to_vec(),
            _ => return None,
        })
    }
}

impl FileSystem for ProcFs {
    fn open(&self, path: &str, _flags: u64, _mode: u32) -> Result<Arc<dyn FileHandle>, Errno> {
        let path = self.canonical(path);
        if let Some(data) = self.generate(&path) {
            return Ok(Arc::new(Generated { data }));
        }
        let entries = match path.as_str() {
            "" => vec!["self", "meminfo", "cpuinfo", "version", "uptime", "sys"],
            "self" => vec![
                "maps", "cmdline", "environ", "stat", "status", "comm", "exe", "cwd", "fd",
            ],
            "self/fd" => {
                let mut out = base_entries();
                for (fd, _) in self.source.fds() {
                    out.push(DirEntry {
                        name: fd.to_string(),
                        file_type: FileType::Symlink,
                        ino: 1,
                    });
                }
                return Ok(Arc::new(GenDir { entries: out }));
            }
            "sys" => vec!["kernel", "vm"],
            "sys/kernel" => vec!["osrelease", "pid_max"],
            "sys/vm" => vec!["overcommit_memory"],
            _ => return Err(Errno::ENOENT),
        };
        let mut out = base_entries();
        for name in entries {
            out.push(DirEntry {
                name: name.to_string(),
                file_type: FileType::Regular,
                ino: 1,
            });
        }
        Ok(Arc::new(GenDir { entries: out }))
    }

    fn metadata(&self, path: &str) -> Result<Metadata, Errno> {
        let path = self.canonical(path);
        if self.generate(&path).is_some() {
            return Ok(Metadata {
                file_type: FileType::Regular,
                size: 0,
                mode: 0o444,
                ino: 1,
                mtime: 0,
                nlink: 1,
            });
        }
        match path.as_str() {
            "" | "self" | "self/fd" | "sys" | "sys/kernel" | "sys/vm" => Ok(Metadata::dir()),
            "self/exe" | "self/cwd" => Ok(Metadata {
                file_type: FileType::Symlink,
                size: 0,
                mode: 0o777,
                ino: 1,
                mtime: 0,
                nlink: 1,
            }),
            _ => Err(Errno::ENOENT),
        }
    }

    fn read_link(&self, path: &str) -> Result<String, Errno> {
        let path = self.canonical(path);
        match path.as_str() {
            "self/exe" => Ok(self.source.exe()),
            "self/cwd" => Ok(self.source.cwd()),
            other => {
                if let Some(n) = other.strip_prefix("self/fd/") {
                    let fd: i32 = n.parse().map_err(|_| Errno::ENOENT)?;
                    for (open_fd, target) in self.source.fds() {
                        if open_fd == fd {
                            return Ok(target);
                        }
                    }
                    return Err(Errno::ENOENT);
                }
                Err(Errno::EINVAL)
            }
        }
    }
}

fn base_entries() -> Vec<DirEntry> {
    vec![
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
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake;
    impl ProcSource for Fake {
        fn pid(&self) -> i32 {
            42
        }
        fn maps(&self) -> String {
            "00400000-00401000 r-xp 00000000 00:00 0 [image]\n".into()
        }
        fn cmdline(&self) -> Vec<u8> {
            b"/bin/prog\0arg\0".to_vec()
        }
        fn environ(&self) -> Vec<u8> {
            b"PATH=/bin\0".to_vec()
        }
        fn exe(&self) -> String {
            "/bin/prog".into()
        }
        fn cwd(&self) -> String {
            "/tmp".into()
        }
        fn fds(&self) -> Vec<(i32, String)> {
            vec![(0, "/dev/tty".into()), (3, "/tmp/x".into())]
        }
        fn stat(&self) -> String {
            "42 (prog) R 1 42 42 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 0\n".into()
        }
        fn status(&self) -> String {
            "Name:\tprog\nPid:\t42\n".into()
        }
    }

    fn fs() -> ProcFs {
        ProcFs::new(Arc::new(Fake))
    }

    #[test]
    fn maps_comes_from_live_state() {
        let f = fs().open("self/maps", 0, 0).unwrap();
        let mut buf = [0u8; 128];
        let n = f.read_at(0, &mut buf).unwrap() as usize;
        assert!(String::from_utf8_lossy(&buf[..n]).contains("[image]"));
    }

    #[test]
    fn numeric_pid_paths_alias_self() {
        let f = fs().open("42/cmdline", 0, 0).unwrap();
        let mut buf = [0u8; 32];
        let n = f.read_at(0, &mut buf).unwrap() as usize;
        assert_eq!(&buf[..n], b"/bin/prog\0arg\0");
    }

    #[test]
    fn exe_and_fd_links_resolve() {
        assert_eq!(fs().read_link("self/exe").unwrap(), "/bin/prog");
        assert_eq!(fs().read_link("self/fd/3").unwrap(), "/tmp/x");
        assert_eq!(fs().read_link("self/fd/9").err(), Some(Errno::ENOENT));
    }

    #[test]
    fn reads_resume_from_an_offset() {
        let f = fs().open("self/maps", 0, 0).unwrap();
        let mut buf = [0u8; 4];
        f.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"0040");
        f.read_at(4, &mut buf).unwrap();
        assert_eq!(&buf, b"0000");
    }
}
