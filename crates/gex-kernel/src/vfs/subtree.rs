//! A directory of another filesystem as a filesystem of its own: the root a
//! `chroot` gives a namespace ([`super::Namespace::chroot`]).
//!
//! Every path is the inner filesystem's with the directory in front. Links
//! are read here and followed by the walk this filesystem does as a root
//! ([`super::walk_links`]), so an absolute link target resolves from the
//! new root, never the old one, and `..` stops at it.

use super::{FileHandle, FileSystem, Metadata, Walked, XattrOp};
use crate::errno::Errno;
use std::sync::Arc;

pub struct Subtree {
    inner: Arc<dyn FileSystem>,
    /// The directory, relative to the inner filesystem's root, no leading
    /// or trailing slash, never empty.
    dir: String,
}

impl Subtree {
    pub fn new(inner: Arc<dyn FileSystem>, dir: &str) -> Subtree {
        Subtree { inner, dir: dir.trim_matches('/').to_string() }
    }

    fn at(&self, path: &str) -> String {
        let path = path.trim_start_matches('/');
        if path.is_empty() {
            self.dir.clone()
        } else {
            format!("{}/{path}", self.dir)
        }
    }
}

impl FileSystem for Subtree {
    fn fs_type(&self) -> &'static str {
        self.inner.fs_type()
    }
    fn open(&self, path: &str, flags: u64, mode: u32) -> Result<Arc<dyn FileHandle>, Errno> {
        self.inner.open(&self.at(path), flags, mode)
    }
    fn open_mode(&self, path: &str, flags: u64, mode: u32) -> Result<Arc<dyn FileHandle>, Errno> {
        self.inner.open_mode(&self.at(path), flags, mode)
    }
    fn metadata(&self, path: &str) -> Result<Metadata, Errno> {
        self.inner.metadata(&self.at(path))
    }
    fn metadata_nofollow(&self, path: &str) -> Result<Metadata, Errno> {
        self.inner.metadata_nofollow(&self.at(path))
    }
    fn access(&self, path: &str, mode: u32) -> Result<(), Errno> {
        self.inner.access(&self.at(path), mode)
    }
    fn read_link(&self, path: &str) -> Result<String, Errno> {
        self.inner.read_link(&self.at(path))
    }
    // `walk` and `walk_metadata` are the trait's, by `read_link`: the inner
    // filesystem's own walk (one round trip for the browser's) would follow
    // an absolute link from its root.
    fn walk(&self, abs: &str, follow_final: bool, mounts: &str) -> Result<Walked, Errno> {
        super::walk_links(abs.to_string(), follow_final, mounts, &|rel| self.read_link(rel))
    }
    fn unlink(&self, path: &str) -> Result<(), Errno> {
        self.inner.unlink(&self.at(path))
    }
    fn mkdir(&self, path: &str, mode: u32) -> Result<(), Errno> {
        self.inner.mkdir(&self.at(path), mode)
    }
    fn rmdir(&self, path: &str) -> Result<(), Errno> {
        self.inner.rmdir(&self.at(path))
    }
    fn rename(&self, from: &str, to: &str) -> Result<(), Errno> {
        self.inner.rename(&self.at(from), &self.at(to))
    }
    fn rename2(&self, from: &str, to: &str, noreplace: bool) -> Result<(), Errno> {
        self.inner.rename2(&self.at(from), &self.at(to), noreplace)
    }
    fn chmod(&self, path: &str, mode: u32) -> Result<(), Errno> {
        self.inner.chmod(&self.at(path), mode)
    }
    fn set_mtime(&self, path: &str, secs: i64) -> Result<(), Errno> {
        self.inner.set_mtime(&self.at(path), secs)
    }
    fn set_times(&self, path: &str, atime: Option<(i64, u32)>, mtime: Option<(i64, u32)>) -> Result<(), Errno> {
        self.inner.set_times(&self.at(path), atime, mtime)
    }
    fn link(&self, from: &str, to: &str) -> Result<(), Errno> {
        self.inner.link(&self.at(from), &self.at(to))
    }
    fn symlink(&self, target: &str, path: &str) -> Result<(), Errno> {
        self.inner.symlink(target, &self.at(path))
    }
    fn mksocket(&self, path: &str) -> Result<(), Errno> {
        self.inner.mksocket(&self.at(path))
    }
    fn watch_key(&self, path: &str) -> Result<(Option<crate::inotify::Key>, bool), Errno> {
        self.inner.watch_key(&self.at(path))
    }
    // Not `watched_remotely`: a watch kept by path elsewhere would be by
    // the chrooted path, which names another file there.
    fn xattr(&self, path: &str, op: XattrOp<'_>) -> Result<usize, Errno> {
        self.inner.xattr(&self.at(path), op)
    }
}
