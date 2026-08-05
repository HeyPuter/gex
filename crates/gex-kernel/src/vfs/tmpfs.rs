//! An in-memory filesystem.
//!
//! This is the writable layer for everything the guest creates and the one
//! backend that works identically on every host, so it is what `/tmp`,
//! `/dev/shm` and an overlay's upper layer are built from.

use super::{DirEntry, FileHandle, FileSystem, FileType, Metadata};
use crate::errno::{Errno, SysResult};
use crate::fs::oflags;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// A file's contents, held separately from any name for it.
///
/// This is what makes an open descriptor survive `unlink`, which is not a
/// nicety: SQLite creates its temporary files, unlinks them immediately and
/// then writes through the descriptor it still holds. A handle that resolved
/// its path on every access answered `ENOENT`, which SQLite reports as a disk
/// I/O error.
#[derive(Debug, Default)]
pub struct FileNode {
    data: Mutex<Vec<u8>>,
    ino: u64,
}

#[derive(Debug)]
enum Node {
    File(Arc<FileNode>),
    Dir,
    Symlink(String),
}

#[derive(Debug, Default)]
struct Inner {
    /// Path (relative to the mount, no leading slash) to node.
    nodes: BTreeMap<String, Node>,
    next_ino: u64,
    inos: BTreeMap<String, u64>,
}

/// An in-memory filesystem.
pub struct TmpFs {
    inner: Arc<Mutex<Inner>>,
}

impl TmpFs {
    pub fn new() -> TmpFs {
        let mut inner = Inner {
            next_ino: 2,
            ..Default::default()
        };
        inner.nodes.insert(String::new(), Node::Dir);
        inner.inos.insert(String::new(), 1);
        TmpFs {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    /// Pre-populate a file, for seeding a root filesystem at startup.
    pub fn add_file(&self, path: &str, contents: Vec<u8>) {
        let path = path.trim_start_matches('/').to_string();
        let mut inner = self.inner.lock().unwrap();
        Self::ensure_parents(&mut inner, &path);
        let ino = inner.next_ino;
        inner.next_ino += 1;
        inner.inos.insert(path.clone(), ino);
        inner.nodes.insert(
            path,
            Node::File(Arc::new(FileNode {
                data: Mutex::new(contents),
                ino,
            })),
        );
    }

    pub fn add_dir(&self, path: &str) {
        let path = path.trim_start_matches('/').to_string();
        let mut inner = self.inner.lock().unwrap();
        Self::ensure_parents(&mut inner, &path);
        let ino = inner.next_ino;
        inner.next_ino += 1;
        inner.inos.entry(path.clone()).or_insert(ino);
        inner.nodes.entry(path).or_insert(Node::Dir);
    }

    pub fn add_symlink(&self, path: &str, target: &str) {
        let path = path.trim_start_matches('/').to_string();
        let mut inner = self.inner.lock().unwrap();
        Self::ensure_parents(&mut inner, &path);
        let ino = inner.next_ino;
        inner.next_ino += 1;
        inner.inos.insert(path.clone(), ino);
        inner.nodes.insert(path, Node::Symlink(target.to_string()));
    }

    /// Create every missing directory above `path`.
    fn ensure_parents(inner: &mut Inner, path: &str) {
        let mut acc = String::new();
        let parts: Vec<&str> = path.split('/').collect();
        for part in &parts[..parts.len().saturating_sub(1)] {
            if part.is_empty() {
                continue;
            }
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            if !inner.nodes.contains_key(&acc) {
                let ino = inner.next_ino;
                inner.next_ino += 1;
                inner.inos.insert(acc.clone(), ino);
                inner.nodes.insert(acc.clone(), Node::Dir);
            }
        }
    }

    fn ino_of(inner: &Inner, path: &str) -> u64 {
        inner.inos.get(path).copied().unwrap_or(1)
    }
}

impl Default for TmpFs {
    fn default() -> Self {
        Self::new()
    }
}

/// A handle onto one tmpfs node.
#[derive(Debug)]
struct TmpHandle {
    inner: Arc<Mutex<Inner>>,
    path: String,
    /// The contents, when this handle is onto a file. Held directly, so the
    /// descriptor keeps working after the name is unlinked or renamed.
    file: Option<Arc<FileNode>>,
}

impl FileHandle for TmpHandle {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> SysResult {
        let Some(file) = &self.file else {
            return Err(Errno::EISDIR);
        };
        let data = file.data.lock().unwrap();
        let start = (offset as usize).min(data.len());
        let n = buf.len().min(data.len() - start);
        buf[..n].copy_from_slice(&data[start..start + n]);
        Ok(n as u64)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> SysResult {
        let Some(file) = &self.file else {
            return Err(Errno::EISDIR);
        };
        let mut data = file.data.lock().unwrap();
        let start = offset as usize;
        if start + buf.len() > data.len() {
            data.resize(start + buf.len(), 0);
        }
        data[start..start + buf.len()].copy_from_slice(buf);
        Ok(buf.len() as u64)
    }

    fn metadata(&self) -> Result<Metadata, Errno> {
        // An unlinked file still has a size and an inode number, so a handle
        // onto one answers from the contents rather than the tree.
        if let Some(file) = &self.file {
            return Ok(Metadata {
                file_type: FileType::Regular,
                size: file.data.lock().unwrap().len() as u64,
                mode: 0o644,
                ino: file.ino,
                mtime: 0,
            });
        }
        let inner = self.inner.lock().unwrap();
        let ino = TmpFs::ino_of(&inner, &self.path);
        match inner.nodes.get(&self.path) {
            Some(Node::File(f)) => Ok(Metadata {
                file_type: FileType::Regular,
                size: f.data.lock().unwrap().len() as u64,
                mode: 0o644,
                ino,
                mtime: 0,
            }),
            Some(Node::Dir) => Ok(Metadata {
                ino,
                ..Metadata::dir()
            }),
            Some(Node::Symlink(t)) => Ok(Metadata {
                file_type: FileType::Symlink,
                size: t.len() as u64,
                mode: 0o777,
                ino,
                mtime: 0,
            }),
            None => Err(Errno::ENOENT),
        }
    }

    fn read_dir(&self) -> Result<Vec<DirEntry>, Errno> {
        let inner = self.inner.lock().unwrap();
        if !matches!(inner.nodes.get(&self.path), Some(Node::Dir)) {
            return Err(Errno::ENOTDIR);
        }
        let prefix = if self.path.is_empty() {
            String::new()
        } else {
            format!("{}/", self.path)
        };
        let mut out = vec![
            DirEntry {
                name: ".".into(),
                file_type: FileType::Directory,
                ino: TmpFs::ino_of(&inner, &self.path),
            },
            DirEntry {
                name: "..".into(),
                file_type: FileType::Directory,
                ino: 1,
            },
        ];
        for (path, node) in &inner.nodes {
            let Some(rest) = path.strip_prefix(&prefix) else {
                continue;
            };
            // Only immediate children.
            if rest.is_empty() || rest.contains('/') {
                continue;
            }
            out.push(DirEntry {
                name: rest.to_string(),
                file_type: match node {
                    Node::File(_) => FileType::Regular,
                    Node::Dir => FileType::Directory,
                    Node::Symlink(_) => FileType::Symlink,
                },
                ino: TmpFs::ino_of(&inner, path),
            });
        }
        Ok(out)
    }

    fn truncate(&self, len: u64) -> Result<(), Errno> {
        let Some(file) = &self.file else {
            return Err(Errno::EINVAL);
        };
        file.data.lock().unwrap().resize(len as usize, 0);
        Ok(())
    }
}

impl FileSystem for TmpFs {
    fn open(&self, path: &str, flags: u64, _mode: u32) -> Result<Arc<dyn FileHandle>, Errno> {
        let path = path.to_string();
        let file = {
            let mut inner = self.inner.lock().unwrap();
            match inner.nodes.get(&path) {
                Some(_) => {
                    if flags & oflags::O_EXCL != 0 && flags & oflags::O_CREAT != 0 {
                        return Err(Errno::EEXIST);
                    }
                    if flags & oflags::O_TRUNC != 0 {
                        if let Some(Node::File(f)) = inner.nodes.get(&path) {
                            f.data.lock().unwrap().clear();
                        }
                    }
                    match inner.nodes.get(&path) {
                        Some(Node::File(f)) => Some(Arc::clone(f)),
                        _ => None,
                    }
                }
                None => {
                    if flags & oflags::O_CREAT == 0 {
                        return Err(Errno::ENOENT);
                    }
                    // The parent must exist, as it must on a real filesystem.
                    let parent = match path.rfind('/') {
                        Some(i) => path[..i].to_string(),
                        None => String::new(),
                    };
                    if !matches!(inner.nodes.get(&parent), Some(Node::Dir)) {
                        return Err(Errno::ENOENT);
                    }
                    let ino = inner.next_ino;
                    inner.next_ino += 1;
                    inner.inos.insert(path.clone(), ino);
                    let f = Arc::new(FileNode {
                        data: Mutex::new(Vec::new()),
                        ino,
                    });
                    inner.nodes.insert(path.clone(), Node::File(Arc::clone(&f)));
                    Some(f)
                }
            }
        };
        Ok(Arc::new(TmpHandle {
            inner: Arc::clone(&self.inner),
            path,
            file,
        }))
    }

    fn metadata(&self, path: &str) -> Result<Metadata, Errno> {
        let handle = TmpHandle {
            inner: Arc::clone(&self.inner),
            path: path.to_string(),
            file: None,
        };
        handle.metadata()
    }

    fn read_link(&self, path: &str) -> Result<String, Errno> {
        let inner = self.inner.lock().unwrap();
        match inner.nodes.get(path) {
            Some(Node::Symlink(t)) => Ok(t.clone()),
            _ => Err(Errno::EINVAL),
        }
    }

    fn unlink(&self, path: &str) -> Result<(), Errno> {
        let mut inner = self.inner.lock().unwrap();
        match inner.nodes.get(path) {
            Some(Node::Dir) => Err(Errno::EISDIR),
            Some(_) => {
                inner.nodes.remove(path);
                inner.inos.remove(path);
                Ok(())
            }
            None => Err(Errno::ENOENT),
        }
    }

    fn mkdir(&self, path: &str, _mode: u32) -> Result<(), Errno> {
        let mut inner = self.inner.lock().unwrap();
        if inner.nodes.contains_key(path) {
            return Err(Errno::EEXIST);
        }
        let ino = inner.next_ino;
        inner.next_ino += 1;
        inner.inos.insert(path.to_string(), ino);
        inner.nodes.insert(path.to_string(), Node::Dir);
        Ok(())
    }

    fn rmdir(&self, path: &str) -> Result<(), Errno> {
        let mut inner = self.inner.lock().unwrap();
        if !matches!(inner.nodes.get(path), Some(Node::Dir)) {
            return Err(Errno::ENOTDIR);
        }
        let prefix = format!("{path}/");
        if inner.nodes.keys().any(|k| k.starts_with(&prefix)) {
            return Err(Errno::ENOTEMPTY);
        }
        inner.nodes.remove(path);
        inner.inos.remove(path);
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), Errno> {
        let mut inner = self.inner.lock().unwrap();
        let Some(node) = inner.nodes.remove(from) else {
            return Err(Errno::ENOENT);
        };
        let ino = inner.inos.remove(from).unwrap_or(1);
        inner.nodes.insert(to.to_string(), node);
        inner.inos.insert(to.to_string(), ino);
        Ok(())
    }

    fn symlink(&self, target: &str, path: &str) -> Result<(), Errno> {
        let mut inner = self.inner.lock().unwrap();
        if inner.nodes.contains_key(path) {
            return Err(Errno::EEXIST);
        }
        let ino = inner.next_ino;
        inner.next_ino += 1;
        inner.inos.insert(path.to_string(), ino);
        inner
            .nodes
            .insert(path.to_string(), Node::Symlink(target.to_string()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_write_read_roundtrip() {
        let fs = TmpFs::new();
        let f = fs
            .open("hello.txt", oflags::O_CREAT | oflags::O_RDWR, 0o644)
            .unwrap();
        assert_eq!(f.write_at(0, b"gex").unwrap(), 3);
        let mut buf = [0u8; 8];
        assert_eq!(f.read_at(0, &mut buf).unwrap(), 3);
        assert_eq!(&buf[..3], b"gex");
        assert_eq!(f.metadata().unwrap().size, 3);
    }

    #[test]
    fn writing_past_the_end_extends_with_zeros() {
        let fs = TmpFs::new();
        let f = fs.open("f", oflags::O_CREAT | oflags::O_RDWR, 0).unwrap();
        f.write_at(4, b"xy").unwrap();
        let mut buf = [0xffu8; 6];
        assert_eq!(f.read_at(0, &mut buf).unwrap(), 6);
        assert_eq!(&buf, b"\0\0\0\0xy");
    }

    #[test]
    fn opening_a_missing_file_without_creat_fails() {
        let fs = TmpFs::new();
        assert_eq!(fs.open("nope", 0, 0).err(), Some(Errno::ENOENT));
    }

    #[test]
    fn directories_list_only_immediate_children() {
        let fs = TmpFs::new();
        fs.add_file("a/b/deep.txt", b"x".to_vec());
        fs.add_file("a/top.txt", b"y".to_vec());
        let d = fs.open("a", 0, 0).unwrap();
        let names: Vec<String> = d.read_dir().unwrap().into_iter().map(|e| e.name).collect();
        assert!(names.contains(&"b".to_string()));
        assert!(names.contains(&"top.txt".to_string()));
        assert!(!names.contains(&"deep.txt".to_string()));
    }

    #[test]
    fn rmdir_refuses_a_non_empty_directory() {
        let fs = TmpFs::new();
        fs.add_file("d/f", b"x".to_vec());
        assert_eq!(fs.rmdir("d").err(), Some(Errno::ENOTEMPTY));
        fs.unlink("d/f").unwrap();
        assert!(fs.rmdir("d").is_ok());
    }

    #[test]
    fn an_unlinked_file_stays_usable_through_an_open_descriptor() {
        // What SQLite does with its temporary files: create, unlink at once,
        // then keep writing through the descriptor it still holds.
        let fs = TmpFs::new();
        let f = fs
            .open("scratch", oflags::O_CREAT | oflags::O_RDWR, 0o600)
            .unwrap();
        fs.unlink("scratch").unwrap();
        assert_eq!(fs.metadata("scratch").err(), Some(Errno::ENOENT));
        assert_eq!(f.write_at(0, b"still here").unwrap(), 10);
        let mut buf = [0u8; 10];
        assert_eq!(f.read_at(0, &mut buf).unwrap(), 10);
        assert_eq!(&buf, b"still here");
        assert_eq!(f.metadata().unwrap().size, 10);
    }

    #[test]
    fn renaming_does_not_disturb_an_open_descriptor() {
        let fs = TmpFs::new();
        let f = fs.open("a", oflags::O_CREAT | oflags::O_RDWR, 0).unwrap();
        f.write_at(0, b"xyz").unwrap();
        fs.rename("a", "b").unwrap();
        let mut buf = [0u8; 3];
        assert_eq!(f.read_at(0, &mut buf).unwrap(), 3);
        assert_eq!(&buf, b"xyz");
    }

    #[test]
    fn symlinks_report_their_target() {
        let fs = TmpFs::new();
        fs.add_file("real", b"data".to_vec());
        fs.symlink("real", "link").unwrap();
        assert_eq!(fs.read_link("link").unwrap(), "real");
        assert_eq!(fs.read_link("real").err(), Some(Errno::EINVAL));
    }
}
