//! A tar laid into the root from the page (web/runtime kfs.js `addTar`),
//! sent in parts as it is fetched and unpacked (`gex_kernel::tar::Reader`):
//! directories, files, modes, times, symbolic and hard links, extended
//! attributes. Over whatever is there: a later tar's path wins. Seeding,
//! not a program's writes, so persistence does not take it.

use std::sync::Mutex;

use gex_kernel::tar::{Event, Header, Kind, Reader};
use gex_kernel::vfs::{FileSystem, XattrOp};

use crate::rootfs;

struct Ingest {
    reader: Reader,
    /// Where the tar's top goes, without its leading slash.
    at: String,
    /// The file whose bytes are coming.
    file: Option<(Header, Vec<u8>)>,
    files: u32,
}

static INGESTS: Mutex<Vec<Option<Ingest>>> = Mutex::new(Vec::new());

fn under(at: &str, p: &str) -> String {
    match (at.is_empty(), p.is_empty()) {
        (true, _) => p.to_string(),
        (false, true) => at.to_string(),
        _ => format!("{at}/{p}"),
    }
}

/// What every entry has: its mode, time and extended attributes.
fn finish(p: &str, h: &Header) {
    let fs = rootfs();
    if h.mode != 0 && h.kind != Kind::Symlink {
        let _ = fs.chmod(p, h.mode);
    }
    if h.kind != Kind::Symlink {
        let _ = fs.set_mtime(p, h.mtime);
    }
    for (name, value) in &h.xattrs {
        let _ = fs.xattr(p, XattrOp::Set { name, value, flags: 0 });
    }
}

fn entry(g: &mut Ingest, h: Header) {
    let fs = rootfs();
    let p = under(&g.at, &h.path);
    let parent = || {
        if let Some((d, _)) = p.rsplit_once('/') {
            fs.add_dir(d);
        }
    };
    match h.kind {
        Kind::Dir => {
            if !p.is_empty() {
                fs.add_dir(&p);
            }
            finish(&p, &h);
        }
        Kind::File if h.size == 0 => {
            fs.add_file(&p, Vec::new());
            finish(&p, &h);
            g.files += 1;
        }
        Kind::File => g.file = Some((h, Vec::new())),
        Kind::Symlink => {
            parent();
            let _ = fs.unlink(&p);
            let _ = fs.symlink(&h.link, &p);
            finish(&p, &h);
        }
        Kind::Hard => {
            parent();
            let _ = fs.unlink(&p);
            let _ = fs.link(&under(&g.at, &h.link), &p);
        }
        Kind::Other => {}
    }
}

fn data(g: &mut Ingest, d: &[u8]) {
    let Some((h, bytes)) = g.file.as_mut() else { return };
    if bytes.is_empty() {
        bytes.reserve_exact(h.size as usize);
    }
    bytes.extend_from_slice(d);
    if bytes.len() as u64 >= h.size {
        let (h, bytes) = g.file.take().unwrap();
        let p = under(&g.at, &h.path);
        rootfs().add_file(&p, bytes);
        finish(&p, &h);
        g.files += 1;
    }
}

/// A tar to come, laid at `at`. Its id.
#[no_mangle]
pub extern "C" fn k_fs_tar_open(at: gex_kmem::PAddr, at_len: u32) -> i32 {
    let at = crate::path_of(at, at_len).trim_end_matches('/').to_string();
    let g = Ingest { reader: Reader::default(), at, file: None, files: 0 };
    let mut t = INGESTS.lock().unwrap_or_else(|e| e.into_inner());
    match t.iter().position(Option::is_none) {
        Some(i) => {
            t[i] = Some(g);
            i as i32
        }
        None => {
            t.push(Some(g));
            t.len() as i32 - 1
        }
    }
}

/// The next `len` bytes of tar `id`, laid in as far as they go.
#[no_mangle]
pub extern "C" fn k_fs_tar_feed(id: i32, at: gex_kmem::PAddr, len: u32) -> i32 {
    let mut part = vec![0u8; len as usize];
    if len > 0 {
        unsafe { gex_kmem::pull_checked(part.as_mut_ptr(), at, len) };
    }
    let mut t = INGESTS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(Some(g)) = t.get_mut(id as usize) else { return -9 };
    let mut reader = std::mem::take(&mut g.reader);
    reader.feed(&part, &mut |e| match e {
        Event::Entry(h) => entry(g, h),
        Event::Data(d) => data(g, d),
    });
    g.reader = reader;
    0
}

/// The end of tar `id`: the count of files it laid in.
#[no_mangle]
pub extern "C" fn k_fs_tar_close(id: i32) -> i32 {
    let mut t = INGESTS.lock().unwrap_or_else(|e| e.into_inner());
    match t.get_mut(id as usize).and_then(Option::take) {
        Some(g) => g.files as i32,
        None => -9,
    }
}
