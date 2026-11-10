//! The root kept across loads in its block store (an `OPFSFilesystem`, a
//! page's own store): what the programs changed of the tree as the sources
//! laid it, written into the store beside the files' blocks, and laid over
//! the sources again at the next boot.
//!
//! The store's block 0 holds two superblocks, written in turn, each naming
//! a generation: its number, the newest block of its records and how many
//! blocks those are. The records are a chain of blocks, each naming the one
//! before it, each with a checksum: a checkpoint (every record) and then a
//! segment per generation since (the records that changed, and the paths
//! that no longer have one). A record is a path's difference from the
//! sources: gone, a directory, a symlink, a file of the sources with its
//! mode and extended attributes changed (its bytes still the sources'), or
//! a file with its bytes (a small one) or its blocks in the store.
//!
//! A generation is written at `sync(2)`, `syncfs`, `fsync`, `fdatasync`, the
//! page's sync, and by the write-back ([`tick`]) once something has waited
//! its age: the dirty blocks first, then the records, a flush, then the
//! superblock, a flush. A block freed meanwhile is not used again until the
//! generation after the one that last named it is written, so the one before
//! stays whole. A boot reads the newest superblock whose records all check;
//! one torn or failing checks falls back to the one before, and says so.
//! The files' bytes are not journalled: after a fall back, a file reads
//! what was last written into the blocks it had.

use super::{FileNode, Inner, Lazy, Node, TmpFs, META};
use crate::errno::Errno;
use crate::vfs::blocks;
use crate::vfs::Metadata;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const SB_MAGIC: &[u8; 8] = b"gexroot\x01";
const SB_LEN: usize = 60;
/// Superblock `g` is at `(g % 2)` halves of block 0.
fn sb_at(gen: u64) -> u64 {
    (gen % 2) * (blocks::block() / 2)
}
const CHAIN_MAGIC: u32 = 0x4252_5847; // "GXRB"
const HDR: usize = 32;
/// The journal is folded into a new checkpoint once its records are more
/// bytes than the last checkpoint's, or once it takes this many blocks
/// (each generation's segment takes a block of its own at least, written
/// only as far as it is used): a checkpoint is written once per its own
/// size of journal, not once per its size in generations.
const JOURNAL_MAX: u64 = 512;

const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// CRC-32 (IEEE).
pub fn crc32(b: &[u8]) -> u32 {
    let mut c = !0u32;
    for &x in b {
        c = CRC_TABLE[((c ^ x as u32) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

/// FNV-1a: a record's signature, which says whether it changed.
fn fnv(h: u64, b: &[u8]) -> u64 {
    let mut h = h;
    for &x in b {
        h = (h ^ x as u64).wrapping_mul(0x0100_0000_01b3);
    }
    h
}
const FNV0: u64 = 0xcbf2_9ce4_8422_2325;

type Xattrs = Vec<(String, Vec<u8>)>;

#[derive(Debug, Clone, PartialEq)]
enum Body {
    Inline(Vec<u8>),
    /// Size and the store's blocks (each plus one, 0 a hole).
    Blocks(u64, Vec<u32>),
    /// A lazy file still fetched from its URL: URL and size.
    Lazy(String, u64),
}

#[derive(Debug, Clone, PartialEq)]
enum Rec {
    /// A path of the sources removed, and all under it.
    Gone,
    Dir { mode: u32, mtime: i64, xattrs: Xattrs },
    Link { target: String },
    /// A file of the sources, its mode changed (read from older stores;
    /// [`Rec::Attrs`] is written instead).
    Mode { mode: u32 },
    /// A file of the sources, its mode or extended attributes changed, its
    /// bytes not, or a file of a layer laid after boot that has extended
    /// attributes: laid over the sources' file, or over the layer's when it
    /// is laid again ([`late_laid`]), so a site's newer file keeps what was
    /// set on the old one (a `user.gex.mem64` record).
    Attrs { mode: u32, xattrs: Xattrs },
    /// `ino` names the file among this generation's records (hard links).
    File { ino: u64, mode: u32, mtime: i64, body: Body, xattrs: Xattrs },
}

/// A record as the walk finds it, its file's bytes not yet taken.
enum Pre {
    Gone,
    Dir { mode: u32, mtime: i64, xattrs: Xattrs },
    Link { target: String },
    Attrs { mode: u32, xattrs: Xattrs },
    File { node: Arc<FileNode>, mode: u32, xattrs: Xattrs },
}

#[derive(Default)]
struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
    fn xattrs(&mut self, x: &Xattrs) {
        self.u32(x.len() as u32);
        for (k, v) in x {
            self.bytes(k.as_bytes());
            self.bytes(v);
        }
    }
    fn rec(&mut self, path: &str, r: &Rec) {
        self.u8(TAG_PUT);
        self.bytes(path.as_bytes());
        match r {
            Rec::Gone => self.u8(0),
            Rec::Dir { mode, mtime, xattrs } => {
                self.u8(1);
                self.u32(*mode);
                self.u64(*mtime as u64);
                self.xattrs(xattrs);
            }
            Rec::Link { target } => {
                self.u8(2);
                self.bytes(target.as_bytes());
            }
            Rec::Mode { mode } => {
                self.u8(3);
                self.u32(*mode);
            }
            Rec::Attrs { mode, xattrs } => {
                self.u8(5);
                self.u32(*mode);
                self.xattrs(xattrs);
            }
            Rec::File { ino, mode, mtime, body, xattrs } => {
                self.u8(4);
                self.u64(*ino);
                self.u32(*mode);
                self.u64(*mtime as u64);
                self.xattrs(xattrs);
                match body {
                    Body::Inline(d) => {
                        self.u8(0);
                        self.bytes(d);
                    }
                    Body::Blocks(size, map) => {
                        self.u8(1);
                        self.u64(*size);
                        self.u32(map.len() as u32);
                        for &b in map {
                            self.u32(b);
                        }
                    }
                    Body::Lazy(url, size) => {
                        self.u8(2);
                        self.bytes(url.as_bytes());
                        self.u64(*size);
                    }
                }
            }
        }
    }
}

const TAG_MARK: u8 = 1;
const TAG_PUT: u8 = 2;
const TAG_DROP: u8 = 3;

struct R<'a>(&'a [u8], usize);

impl R<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], Errno> {
        let b = self.0.get(self.1..self.1 + n).ok_or(Errno::EIO)?;
        self.1 += n;
        Ok(b)
    }
    fn u8(&mut self) -> Result<u8, Errno> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, Errno> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, Errno> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self) -> Result<Vec<u8>, Errno> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    fn str(&mut self) -> Result<String, Errno> {
        String::from_utf8(self.bytes()?).map_err(|_| Errno::EIO)
    }
    fn xattrs(&mut self) -> Result<Xattrs, Errno> {
        let n = self.u32()?;
        (0..n).map(|_| Ok((self.str()?, self.bytes()?))).collect()
    }
    fn rec(&mut self) -> Result<Rec, Errno> {
        Ok(match self.u8()? {
            0 => Rec::Gone,
            1 => Rec::Dir { mode: self.u32()?, mtime: self.u64()? as i64, xattrs: self.xattrs()? },
            2 => Rec::Link { target: self.str()? },
            3 => Rec::Mode { mode: self.u32()? },
            5 => Rec::Attrs { mode: self.u32()?, xattrs: self.xattrs()? },
            4 => {
                let (ino, mode, mtime, xattrs) = (self.u64()?, self.u32()?, self.u64()? as i64, self.xattrs()?);
                let body = match self.u8()? {
                    0 => Body::Inline(self.bytes()?),
                    1 => {
                        let size = self.u64()?;
                        let n = self.u32()?;
                        Body::Blocks(size, (0..n).map(|_| self.u32()).collect::<Result<_, _>>()?)
                    }
                    2 => Body::Lazy(self.str()?, self.u64()?),
                    _ => return Err(Errno::EIO),
                };
                Rec::File { ino, mode, mtime, body, xattrs }
            }
            _ => return Err(Errno::EIO),
        })
    }
}

/// One superblock.
#[derive(Debug, Clone, Copy, Default)]
struct Sb {
    gen: u64,
    /// The newest block of the records, plus one (0: none).
    head: u32,
    /// The store's blocks in use or free (`blocks::high`).
    high: u32,
    /// How many blocks the records take.
    chain: u32,
    /// The checkpoint's bytes and the journal's since.
    ck: u64,
    jb: u64,
}

impl Sb {
    fn encode(&self, bs: u32) -> [u8; SB_LEN] {
        let mut b = [0u8; SB_LEN];
        b[0..8].copy_from_slice(SB_MAGIC);
        b[8..12].copy_from_slice(&1u32.to_le_bytes());
        b[12..16].copy_from_slice(&bs.to_le_bytes());
        b[16..24].copy_from_slice(&self.gen.to_le_bytes());
        b[24..28].copy_from_slice(&self.head.to_le_bytes());
        b[28..32].copy_from_slice(&self.high.to_le_bytes());
        b[32..36].copy_from_slice(&self.chain.to_le_bytes());
        b[40..48].copy_from_slice(&self.ck.to_le_bytes());
        b[48..56].copy_from_slice(&self.jb.to_le_bytes());
        let c = crc32(&b[..56]);
        b[56..60].copy_from_slice(&c.to_le_bytes());
        b
    }
    /// `None`: no superblock here; `Some(Err)`: one, but torn.
    fn decode(b: &[u8], bs: u32) -> Option<Result<Sb, ()>> {
        if &b[0..8] != SB_MAGIC {
            return None;
        }
        let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        if crc32(&b[..56]) != u32_at(56) || u32_at(8) != 1 || u32_at(12) != bs {
            return Some(Err(()));
        }
        Some(Ok(Sb { gen: u64_at(16), head: u32_at(24), high: u32_at(28), chain: u32_at(32), ck: u64_at(40), jb: u64_at(48) }))
    }
}

struct State {
    fs: TmpFs,
    exclude: Vec<String>,
    /// The sources' tree as laid: path to its signature (kind, mode, target).
    seed: HashMap<String, u64>,
    /// What the last generation holds: path to its record's signature.
    committed: HashMap<String, u64>,
    sb: Sb,
    /// The records' blocks, oldest first.
    chain: Vec<u32>,
    /// [`META`] when the last generation was taken.
    seen: u64,
    /// The write-back tick at which the tree was first seen changed since.
    since: Option<u32>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// [`Rec::Attrs`] kept for files the boot did not find: a layer laid after
/// boot (a site's tar added once the system runs) lays them again later,
/// and [`late_laid`] puts what was kept on each as it does. Taken under the
/// tree's lock, never the other way round; never under [`STATE`] first.
static LATE: Mutex<BTreeMap<String, (u32, Xattrs)>> = Mutex::new(BTreeMap::new());

/// A layer laid `path` (inode `ino`) after boot: the mode and extended
/// attributes kept for it ([`LATE`]), if any, go on it. Called with the
/// tree locked, by the laying itself.
pub(super) fn late_laid(inner: &mut Inner, path: &str, ino: u64) {
    let Some((mode, xattrs)) = LATE.lock().unwrap().remove(path) else { return };
    let _ = inner.chmod(path, mode);
    set_xattrs(inner, ino, &xattrs);
}
/// Generations written, records put, paths dropped (k_fs_persist_stats).
pub static GENS: AtomicU64 = AtomicU64::new(0);
pub static PUTS: AtomicU64 = AtomicU64::new(0);
pub static DROPS: AtomicU64 = AtomicU64::new(0);
pub static CHECKPOINTS: AtomicU64 = AtomicU64::new(0);
/// Commits asked for (by a sync, by the tick), paths the walks looked at,
/// records they made, and the metadata bytes written (checkpoints among
/// them): [`stats`].
pub static ST: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
const S_SYNC: usize = 0;
const S_TICK: usize = 1;
const S_NODES: usize = 2;
const S_RECS: usize = 3;
const S_BYTES: usize = 4;
const S_CK_BYTES: usize = 5;
const S_EMPTY: usize = 6;

/// gens, records put, paths dropped, checkpoints; commits by a sync, by
/// the tick; paths walked, records walked; metadata bytes, checkpoint
/// bytes; commits with nothing changed.
pub fn stats() -> [u64; 11] {
    let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
    [
        l(&GENS),
        l(&PUTS),
        l(&DROPS),
        l(&CHECKPOINTS),
        l(&ST[S_SYNC]),
        l(&ST[S_TICK]),
        l(&ST[S_NODES]),
        l(&ST[S_RECS]),
        l(&ST[S_BYTES]),
        l(&ST[S_CK_BYTES]),
        l(&ST[S_EMPTY]),
    ]
}

/// The generation the last boot read back, and what it found (k_fs_restore).
pub static BOOT_GEN: AtomicU64 = AtomicU64::new(0);
pub static LAST_ERR: AtomicI64 = AtomicI64::new(0);

/// What a boot found in the store ([`start`]).
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Found {
    /// Nothing kept: an empty store.
    Fresh,
    /// The newest generation, laid over the sources.
    Restored,
    /// The newest was torn or failed its checks: the one before it.
    FellBack,
    /// Something was kept, but no generation reads whole: started from
    /// the sources alone.
    Unreadable,
}

fn sig_dir(mode: u32, x: &Xattrs) -> u64 {
    let mut h = fnv(FNV0, &[1]);
    h = fnv(h, &mode.to_le_bytes());
    for (k, v) in x {
        h = fnv(fnv(h, k.as_bytes()), v);
    }
    h
}
fn sig_link(t: &str) -> u64 {
    fnv(fnv(FNV0, &[2]), t.as_bytes())
}
fn sig_file_seed(mode: u32, x: &Xattrs) -> u64 {
    let mut h = fnv(fnv(FNV0, &[3]), &mode.to_le_bytes());
    for (k, v) in x {
        h = fnv(fnv(h, k.as_bytes()), v);
    }
    h
}

fn within(p: &str, at: &str) -> bool {
    at.is_empty() || p == at || (p.len() > at.len() && p.starts_with(at) && p.as_bytes()[at.len()] == b'/')
}

fn parent(p: &str) -> &str {
    p.rfind('/').map_or("", |i| &p[..i])
}

fn xattrs_of(inner: &Inner, ino: u64) -> Xattrs {
    inner.xattrs.get(&ino).map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default()
}

fn mode_of(inner: &Inner, p: &str, node: &Node) -> u32 {
    let default = match node {
        Node::Dir => Metadata::dir().mode,
        Node::Symlink(_) => 0o777,
        _ => 0o644,
    };
    inner.modes.get(p).copied().unwrap_or(default) & 0o7777
}

/// The tree's signatures, as the sources laid it.
fn seed_of(fs: &TmpFs) -> HashMap<String, u64> {
    let inner = fs.inner.read().unwrap();
    let mut out = HashMap::with_capacity(inner.nodes.len());
    for (p, n) in inner.nodes.iter() {
        let ino = inner.inos.get(p).copied().unwrap_or(0);
        let s = match n {
            Node::Dir => sig_dir(mode_of(&inner, p, n), &xattrs_of(&inner, ino)),
            Node::Symlink(t) => sig_link(t),
            Node::File(f) => sig_file_seed(mode_of(&inner, p, n), &xattrs_of(&inner, f.ino)),
            Node::Socket => continue,
        };
        out.insert(p.clone(), s);
    }
    out
}

impl State {
    fn excluded(&self, p: &str) -> bool {
        p.is_empty() || self.exclude.iter().any(|at| within(p, at))
    }

    /// Every path that differs from the sources, as records to be.
    fn walk(&self) -> Vec<(String, Pre)> {
        let inner = self.fs.inner.read().unwrap();
        ST[S_NODES].fetch_add((inner.nodes.len() + self.seed.len()) as u64, Ordering::Relaxed);
        let mut out = Vec::new();
        for (p, n) in inner.nodes.iter() {
            if self.excluded(p) {
                continue;
            }
            let ino = inner.inos.get(p).copied().unwrap_or(0);
            let mode = mode_of(&inner, p, n);
            let seed = self.seed.get(p);
            match n {
                Node::Dir => {
                    let xattrs = xattrs_of(&inner, ino);
                    if seed == Some(&sig_dir(mode, &xattrs)) {
                        continue;
                    }
                    out.push((p.clone(), Pre::Dir { mode, mtime: inner.mtimes.get(p).copied().unwrap_or(0), xattrs }));
                }
                Node::Symlink(t) => {
                    if seed == Some(&sig_link(t)) {
                        continue;
                    }
                    out.push((p.clone(), Pre::Link { target: t.clone() }));
                }
                Node::Socket => {}
                Node::File(f) => {
                    let xattrs = xattrs_of(&inner, f.ino);
                    if f.written.load(Ordering::Relaxed) {
                        out.push((p.clone(), Pre::File { node: Arc::clone(f), mode, xattrs }));
                    } else if let Some(&s) = seed {
                        if s != sig_file_seed(mode, &xattrs) {
                            out.push((p.clone(), Pre::Attrs { mode, xattrs }));
                        }
                    } else if !xattrs.is_empty() {
                        // (a layer laid after boot: its bytes are the
                        // layer's, laid again by the next boot's)
                        out.push((p.clone(), Pre::Attrs { mode, xattrs }));
                    }
                    // Not written and not the sources': a layer laid after
                    // boot, laid again by the next one.
                }
            }
        }
        // Kept for a layer not laid again yet.
        for (p, (mode, xattrs)) in LATE.lock().unwrap().iter() {
            if !inner.nodes.contains_key(p) && !self.excluded(p) {
                out.push((p.clone(), Pre::Attrs { mode: *mode, xattrs: xattrs.clone() }));
            }
        }
        // Removed: the topmost of what the sources had.
        for p in self.seed.keys() {
            if inner.nodes.contains_key(p) || self.excluded(p) {
                continue;
            }
            let up = parent(p);
            if !up.is_empty() && !inner.nodes.contains_key(up) && self.seed.contains_key(up) {
                continue;
            }
            out.push((p.clone(), Pre::Gone));
        }
        out
    }

    fn commit(&mut self) -> Result<(), Errno> {
        let seen = META.load(Ordering::Relaxed);
        let pending = blocks::take_pending();
        match self.commit_with(seen) {
            Ok(()) => {
                blocks::release(pending);
                Ok(())
            }
            Err(e) => {
                blocks::repend(pending);
                LAST_ERR.store(-(e.0 as i64), Ordering::Relaxed);
                Err(e)
            }
        }
    }

    fn commit_with(&mut self, seen: u64) -> Result<(), Errno> {
        let walked = self.walk();
        ST[S_RECS].fetch_add(walked.len() as u64, Ordering::Relaxed);
        let mut sigs: HashMap<String, u64> = HashMap::with_capacity(walked.len());
        let mut changed = Vec::new();
        for (p, pre) in &walked {
            let s = sig_of(pre);
            if self.committed.get(p) != Some(&s) {
                changed.push(p.as_str());
            }
            sigs.insert(p.clone(), s);
        }
        let dropped: Vec<&String> = self.committed.keys().filter(|p| !sigs.contains_key(*p)).collect();
        if changed.is_empty() && dropped.is_empty() {
            ST[S_EMPTY].fetch_add(1, Ordering::Relaxed);
            self.seen = seen;
            self.since = None;
            blocks::write_back_all()?;
            return blocks::flush_store();
        }
        let gen = self.sb.gen + 1;
        let mut j = W::default();
        j.u8(TAG_MARK);
        j.u64(gen);
        j.u8(0);
        let by_path: HashMap<&str, &Pre> = walked.iter().map(|(p, r)| (p.as_str(), r)).collect();
        for p in &changed {
            j.rec(p, &take(by_path[p]));
        }
        for p in &dropped {
            j.u8(TAG_DROP);
            j.bytes(p.as_bytes());
        }
        let room = (blocks::block() as usize - HDR) as u64;
        let ck_blocks = self.sb.ck.div_ceil(room);
        let journal = self.sb.chain as u64 - ck_blocks + (j.0.len() as u64).div_ceil(room);
        let fold = self.sb.head == 0 || journal > JOURNAL_MAX || self.sb.jb + j.0.len() as u64 > self.sb.ck.max(room);
        let payload = if fold {
            let mut c = W::default();
            c.u8(TAG_MARK);
            c.u64(gen);
            c.u8(1);
            for (p, pre) in &walked {
                c.rec(p, &take(pre));
            }
            c.0
        } else {
            j.0
        };
        // The files' bytes the records name, then the records.
        blocks::write_back_all()?;
        let prev = if fold { 0 } else { self.sb.head };
        let new = write_chain(&payload, gen, prev)?;
        ST[S_BYTES].fetch_add(payload.len() as u64, Ordering::Relaxed);
        if fold {
            ST[S_CK_BYTES].fetch_add(payload.len() as u64, Ordering::Relaxed);
        }
        let sb = Sb {
            gen,
            head: *new.last().unwrap() + 1,
            high: blocks::high(),
            chain: if fold { new.len() as u32 } else { self.sb.chain + new.len() as u32 },
            ck: if fold { payload.len() as u64 } else { self.sb.ck },
            jb: if fold { 0 } else { self.sb.jb + payload.len() as u64 },
        };
        let wrote = (|| {
            blocks::flush_store()?;
            let mut b = sb.encode(blocks::block() as u32);
            blocks::raw(blocks::OP_WRITE, sb_at(gen), &mut b)?;
            blocks::flush_store()
        })();
        if let Err(e) = wrote {
            blocks::release(new);
            return Err(e);
        }
        if fold {
            blocks::release(std::mem::replace(&mut self.chain, new));
            CHECKPOINTS.fetch_add(1, Ordering::Relaxed);
        } else {
            self.chain.extend(new);
        }
        GENS.fetch_add(1, Ordering::Relaxed);
        PUTS.fetch_add(changed.len() as u64, Ordering::Relaxed);
        DROPS.fetch_add(dropped.len() as u64, Ordering::Relaxed);
        self.sb = sb;
        self.committed = sigs;
        self.seen = seen;
        self.since = None;
        Ok(())
    }
}

/// A record's signature: whether it changed since the last generation,
/// without reading a file's bytes (its count of changes says).
fn sig_of(pre: &Pre) -> u64 {
    match pre {
        Pre::Gone => fnv(FNV0, &[9]),
        Pre::Dir { mode, xattrs, .. } => sig_dir(*mode, xattrs),
        Pre::Link { target } => sig_link(target),
        Pre::Attrs { mode, xattrs } => fnv(sig_file_seed(*mode, xattrs), &[8]),
        Pre::File { node, mode, xattrs } => {
            let mut h = fnv(FNV0, &[4]);
            h = fnv(h, &(Arc::as_ptr(node) as usize as u64).to_le_bytes());
            h = fnv(h, &mode.to_le_bytes());
            h = fnv(h, &node.mtime.load(Ordering::Relaxed).to_le_bytes());
            h = fnv(h, &node.size().to_le_bytes());
            h = fnv(h, &node.changes.load(Ordering::Acquire).to_le_bytes());
            for (k, v) in xattrs {
                h = fnv(fnv(h, k.as_bytes()), v);
            }
            h
        }
    }
}

/// The record itself: a file's bytes (a small one) or its blocks taken now.
/// A large file still in memory (a source's, renamed) moves to the store
/// first, so that its bytes are not copied into the records.
fn take(pre: &Pre) -> Rec {
    match pre {
        Pre::Gone => Rec::Gone,
        Pre::Dir { mode, mtime, xattrs } => Rec::Dir { mode: *mode, mtime: *mtime, xattrs: xattrs.clone() },
        Pre::Link { target } => Rec::Link { target: target.clone() },
        Pre::Attrs { mode, xattrs } => Rec::Attrs { mode: *mode, xattrs: xattrs.clone() },
        Pre::File { node, mode, xattrs } => {
            let mtime = node.mtime.load(Ordering::Relaxed);
            let lazy = node.lazy.lock().unwrap().as_ref().map(|l| (l.url.clone(), l.size));
            let body = if let Some((url, size)) = lazy {
                Body::Lazy(url, size)
            } else {
                let len = node.size();
                let pg = node.paged_for(len);
                match pg.as_ref() {
                    Some(p) => Body::Blocks(p.size, p.blocks().to_vec()),
                    None => Body::Inline(node.data.lock().unwrap().clone()),
                }
            };
            Rec::File { ino: node.ino, mode: *mode, mtime, body, xattrs: xattrs.clone() }
        }
    }
}

/// `payload` as a chain of blocks after `prev` (plus one, 0 none), each
/// marked `gen`: the blocks, oldest first.
fn write_chain(payload: &[u8], gen: u64, prev: u32) -> Result<Vec<u32>, Errno> {
    let bs = blocks::block() as usize;
    let room = bs - HDR;
    let mut out: Vec<u32> = Vec::new();
    let mut prev = prev;
    let mut buf = vec![0u8; bs];
    for part in payload.chunks(room) {
        let b = match blocks::alloc_meta() {
            Ok(b) => b,
            Err(e) => {
                blocks::release(out);
                return Err(e);
            }
        };
        out.push(b);
        let n = HDR + part.len();
        buf[..n].fill(0);
        buf[0..4].copy_from_slice(&CHAIN_MAGIC.to_le_bytes());
        buf[8..16].copy_from_slice(&gen.to_le_bytes());
        buf[16..20].copy_from_slice(&prev.to_le_bytes());
        buf[20..24].copy_from_slice(&(part.len() as u32).to_le_bytes());
        buf[HDR..n].copy_from_slice(part);
        let c = crc32(&buf[8..n]);
        buf[4..8].copy_from_slice(&c.to_le_bytes());
        if let Err(e) = blocks::raw(blocks::OP_WRITE, b as u64 * bs as u64, &mut buf[..n]) {
            blocks::release(out);
            return Err(e);
        }
        prev = b + 1;
    }
    Ok(out)
}

/// The records `sb` names, oldest first, and their blocks; EIO for any
/// block that fails its checks.
fn read_chain(sb: &Sb) -> Result<(Vec<u8>, Vec<u32>), Errno> {
    let bs = blocks::block() as usize;
    let mut parts: Vec<Vec<u8>> = Vec::new();
    let mut chain = Vec::new();
    let mut at = sb.head;
    let mut buf = vec![0u8; bs];
    let mut newest = true;
    while at != 0 {
        if chain.len() as u32 >= sb.chain || at > sb.high {
            return Err(Errno::EIO);
        }
        let b = at - 1;
        blocks::raw(blocks::OP_READ, b as u64 * bs as u64, &mut buf)?;
        let u32_at = |i: usize| u32::from_le_bytes(buf[i..i + 4].try_into().unwrap());
        let gen = u64::from_le_bytes(buf[8..16].try_into().unwrap());
        let len = u32_at(20) as usize;
        if u32_at(0) != CHAIN_MAGIC || len > bs - HDR || crc32(&buf[8..HDR + len]) != u32_at(4) || gen > sb.gen || (newest && gen != sb.gen) {
            return Err(Errno::EIO);
        }
        newest = false;
        parts.push(buf[HDR..HDR + len].to_vec());
        chain.push(b);
        at = u32_at(16);
    }
    if chain.len() as u32 != sb.chain {
        return Err(Errno::EIO);
    }
    parts.reverse();
    chain.reverse();
    Ok((parts.concat(), chain))
}

/// The records a stream leaves: the checkpoint's, then each segment's.
fn replay(stream: &[u8]) -> Result<BTreeMap<String, Rec>, Errno> {
    let mut r = R(stream, 0);
    let mut out = BTreeMap::new();
    let mut first = true;
    while r.1 < stream.len() {
        match r.u8()? {
            TAG_MARK => {
                let _gen = r.u64()?;
                let ck = r.u8()?;
                if first && ck != 1 {
                    return Err(Errno::EIO);
                }
                if ck == 1 {
                    out.clear();
                }
            }
            TAG_PUT => {
                let p = r.str()?;
                let rec = r.rec()?;
                out.insert(p, rec);
            }
            TAG_DROP => {
                out.remove(&r.str()?);
            }
            _ => return Err(Errno::EIO),
        }
        first = false;
    }
    Ok(out)
}

/// Remove `p` and everything under it.
fn remove_tree(inner: &mut Inner, p: &str) {
    let lo = format!("{p}/");
    let hi = format!("{p}0");
    let mut keys: Vec<String> = inner.nodes.range::<String, _>(lo..hi).map(|(k, _)| k.clone()).collect();
    if inner.nodes.contains_key(p) {
        keys.push(p.to_string());
    }
    for k in keys {
        inner.forget(&k);
    }
}

fn set_xattrs(inner: &mut Inner, ino: u64, x: &Xattrs) {
    if x.is_empty() {
        inner.xattrs.remove(&ino);
    } else {
        inner.xattrs.insert(ino, x.iter().cloned().collect());
    }
}

/// Lay `recs` over the tree; the store's blocks they name marked in `used`.
fn apply(fs: &TmpFs, recs: &BTreeMap<String, Rec>, used: &mut Vec<bool>) {
    let mut inner = fs.inner.lock().unwrap();
    for (p, r) in recs {
        if *r == Rec::Gone {
            remove_tree(&mut inner, p);
        }
    }
    let mut files: HashMap<u64, Arc<FileNode>> = HashMap::new();
    let mut mark = |b: u32| {
        if b != 0 {
            let i = (b - 1) as usize;
            if used.len() <= i {
                used.resize(i + 1, false);
            }
            used[i] = true;
        }
    };
    // In path order: a directory before what is in it.
    for (p, r) in recs {
        match r {
            Rec::Gone => {}
            Rec::Mode { mode } => {
                if matches!(inner.nodes.get(p.as_str()), Some(Node::File(_))) {
                    let _ = inner.chmod(p, *mode);
                }
            }
            Rec::Attrs { mode, xattrs } => match inner.nodes.get(p.as_str()) {
                Some(Node::File(f)) => {
                    let ino = f.ino;
                    let _ = inner.chmod(p, *mode);
                    set_xattrs(&mut inner, ino, xattrs);
                }
                // Not laid yet: a layer after boot lays it.
                None => {
                    LATE.lock().unwrap().insert(p.clone(), (*mode, xattrs.clone()));
                }
                Some(_) => {}
            },
            Rec::Dir { mode, mtime, xattrs } => {
                if !matches!(inner.nodes.get(p.as_str()), Some(Node::Dir) | None) {
                    remove_tree(&mut inner, p);
                }
                TmpFs::ensure_parents(&mut inner, p);
                if !inner.nodes.contains_key(p.as_str()) {
                    let ino = inner.next_ino;
                    inner.next_ino += 1;
                    inner.set_ino(p.clone(), ino);
                    inner.nodes.insert(p.clone(), Node::Dir);
                }
                let _ = inner.chmod(p, *mode);
                inner.mtimes.insert(p.clone(), *mtime);
                inner.inferred.remove(p.as_str());
                let ino = inner.inos.get(p.as_str()).copied().unwrap_or(0);
                set_xattrs(&mut inner, ino, xattrs);
            }
            Rec::Link { target } => {
                remove_tree(&mut inner, p);
                TmpFs::ensure_parents(&mut inner, p);
                let ino = inner.next_ino;
                inner.next_ino += 1;
                inner.set_ino(p.clone(), ino);
                inner.nodes.insert(p.clone(), Node::Symlink(target.clone()));
            }
            Rec::File { ino, mode, mtime, body, xattrs } => {
                remove_tree(&mut inner, p);
                TmpFs::ensure_parents(&mut inner, p);
                let node = match files.get(ino) {
                    Some(n) => Arc::clone(n),
                    None => {
                        let new = inner.next_ino;
                        inner.next_ino += 1;
                        let f = match body {
                            Body::Inline(d) => FileNode::new(d.clone(), new, true),
                            Body::Blocks(size, map) => {
                                for &b in map {
                                    mark(b);
                                }
                                let f = FileNode::new(Vec::new(), new, true);
                                *f.pg.lock().unwrap() = Some(blocks::PFile::from_blocks(*size, map.clone()));
                                f
                            }
                            Body::Lazy(url, size) => {
                                let f = FileNode::new(Vec::new(), new, true);
                                *f.lazy.lock().unwrap() = Some(Lazy { url: url.clone(), size: *size, chunks: Default::default(), held: 0, ahead: 0, reads: 0 });
                                f
                            }
                        };
                        f.mtime.store(*mtime, Ordering::Relaxed);
                        f.written.store(true, Ordering::Relaxed);
                        let f = Arc::new(f);
                        files.insert(*ino, Arc::clone(&f));
                        f
                    }
                };
                inner.set_ino(p.clone(), node.ino);
                inner.nodes.insert(p.clone(), Node::File(Arc::clone(&node)));
                let _ = inner.chmod(p, *mode);
                set_xattrs(&mut inner, node.ino, xattrs);
            }
        }
    }
}

/// At boot, after the sources are laid and before the first program: the
/// root `fs` from now on kept in the store (which [`blocks::configure`] set
/// up), and what the store kept laid over it. `exclude`: paths never kept
/// (no leading `/`).
pub fn start(fs: &TmpFs, exclude: Vec<String>) -> Found {
    let fs = TmpFs { inner: Arc::clone(&fs.inner) };
    let seed = seed_of(&fs);
    let bs = blocks::block() as u32;
    let mut head = vec![0u8; bs as usize];
    let read = blocks::raw(blocks::OP_READ, 0, &mut head);
    let slots: Vec<Option<Result<Sb, ()>>> = if read.is_ok() {
        (0..2u64).map(|g| Sb::decode(&head[sb_at(g) as usize..sb_at(g) as usize + SB_LEN], bs)).collect()
    } else {
        vec![Some(Err(())), None]
    };
    let mut ok: Vec<Sb> = slots.iter().filter_map(|s| s.and_then(|r| r.ok())).collect();
    ok.sort_by(|a, b| b.gen.cmp(&a.gen));
    let torn = slots.iter().any(|s| matches!(s, Some(Err(()))));
    let any = slots.iter().any(|s| s.is_some());
    let max_gen = ok.first().map_or(0, |s| s.gen);
    let mut found = if any { Found::Unreadable } else { Found::Fresh };
    let mut st = State { fs, exclude, seed, committed: HashMap::new(), sb: Sb { gen: max_gen, ..Default::default() }, chain: Vec::new(), seen: 0, since: None };
    let mut used = vec![true];
    for (i, sb) in ok.iter().enumerate() {
        let Ok((stream, chain)) = read_chain(sb) else { continue };
        let Ok(recs) = replay(&stream) else { continue };
        for &b in &chain {
            let b = b as usize;
            if used.len() <= b {
                used.resize(b + 1, false);
            }
            used[b] = true;
        }
        apply(&st.fs, &recs, &mut used);
        blocks::rebuild(sb.high, &used);
        // The next generation goes over the other superblock: after a
        // fall back, the one that failed.
        st.sb = *sb;
        st.chain = chain;
        found = if i == 0 && !torn { Found::Restored } else { Found::FellBack };
        BOOT_GEN.store(sb.gen, Ordering::Relaxed);
        break;
    }
    if found != Found::Restored && found != Found::FellBack {
        // Fresh, or nothing whole: the store is used from block 1 up.
        blocks::rebuild(1, &used);
        st.sb.head = 0;
    }
    // What the generation holds, as signatures of the tree now.
    let walked = st.walk();
    st.committed = if st.sb.head == 0 { HashMap::new() } else { walked.iter().map(|(p, pre)| (p.clone(), sig_of(pre))).collect() };
    st.seen = META.load(Ordering::Relaxed);
    blocks::defer_frees();
    *STATE.lock().unwrap() = Some(st);
    found
}

/// `sync(2)`, `syncfs`, `fsync`, `fdatasync`, the page's sync: a generation
/// written, when the root is kept; otherwise the dirty blocks.
pub fn sync() -> Result<(), Errno> {
    let mut g = STATE.lock().unwrap();
    match g.as_mut() {
        Some(st) => {
            ST[S_SYNC].fetch_add(1, Ordering::Relaxed);
            st.commit()
        }
        None => blocks::sync_all(),
    }
}

/// The host's write-back tick: a generation written once the tree has
/// waited `age` ticks since it changed, or a block has been dirty that
/// long. 1 when one was written, 0 when nothing was due.
pub fn tick(age: u32) -> Result<u32, Errno> {
    let mut g = match STATE.try_lock() {
        Ok(g) => g,
        // A sync is writing one now.
        Err(_) => return Ok(0),
    };
    let Some(st) = g.as_mut() else {
        drop(g);
        return blocks::tick(age);
    };
    let now = blocks::next_epoch();
    if META.load(Ordering::Relaxed) != st.seen && st.since.is_none() {
        st.since = Some(now);
    }
    let due = st.since.is_some_and(|s| now.wrapping_sub(s) >= age) || blocks::aged(age);
    if !due {
        return Ok(0);
    }
    ST[S_TICK].fetch_add(1, Ordering::Relaxed);
    st.commit()?;
    Ok(1)
}

/// Whether the root is kept.
pub fn on() -> bool {
    STATE.lock().unwrap().is_some()
}

/// A test's fresh start: the root no longer kept.
#[cfg(test)]
pub fn reset() {
    *STATE.lock().unwrap() = None;
    LATE.lock().unwrap().clear();
    blocks::reset();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::oflags::{O_CREAT, O_RDWR, O_TRUNC};
    use crate::vfs::blocks::tests::{fake, STORE_FILE};
    use crate::vfs::FileSystem;

    const BS: u64 = 4096;

    /// The sources, the same at every boot.
    fn boot() -> TmpFs {
        blocks::set_io(fake);
        *STATE.lock().unwrap() = None;
        blocks::reset();
        blocks::configure(BS as u32, 16 * BS, 512);
        let fs = TmpFs::new();
        fs.use_store();
        fs.add_file("etc/seed", b"seeded".to_vec());
        fs.add_file("etc/big", vec![5u8; 3000]);
        fs.add_file("usr/bin/tool", b"#!".to_vec());
        fs.add_symlink("usr/link", "bin/tool");
        fs.add_dir("var/old");
        fs.add_file("var/old/a", b"a".to_vec());
        fs.add_dir("tmp");
        fs
    }

    fn read(fs: &TmpFs, p: &str) -> Option<Vec<u8>> {
        let h = fs.open(p, O_RDWR, 0).ok()?;
        let mut out = vec![0u8; 1 << 16];
        let n = h.read_at(0, &mut out).ok()? as usize;
        out.truncate(n);
        Some(out)
    }

    fn write(fs: &TmpFs, p: &str, d: &[u8]) {
        let h = fs.open_mode(p, O_CREAT | O_RDWR | O_TRUNC, 0o644).unwrap();
        h.write_at(0, d).unwrap();
    }

    fn big(n: usize, k: u8) -> Vec<u8> {
        (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(k)).collect()
    }

    /// A tree's changes kept across boots: files small and large, a mode,
    /// a removal, a hard link, a symlink and a directory; a torn segment and
    /// a torn superblock each boot the generation before.
    #[test]
    #[ignore]
    fn the_root_is_kept_across_boots_and_a_torn_generation_falls_back() {
        STORE_FILE.lock().unwrap().clear();
        let fs = boot();
        assert_eq!(start(&fs, vec!["tmp".into()]), Found::Fresh);
        fs.mkdir("home", 0o755).unwrap();
        write(&fs, "home/small", b"small file");
        write(&fs, "home/large", &big(40000, 1));
        write(&fs, "etc/seed", b"changed");
        fs.chmod("usr/bin/tool", 0o755).unwrap();
        fs.unlink("usr/link").unwrap();
        fs.unlink("var/old/a").unwrap();
        fs.rmdir("var/old").unwrap();
        fs.link("home/large", "home/again").unwrap();
        fs.symlink("large", "home/sym").unwrap();
        fs.mkdir("tmp/x", 0o755).ok();
        write(&fs, "tmp/scratch", b"never kept");
        sync().unwrap();
        let g1 = STATE.lock().unwrap().as_ref().unwrap().sb.gen;

        let check1 = |fs: &TmpFs| {
            assert_eq!(read(fs, "home/small").unwrap(), b"small file");
            assert_eq!(read(fs, "home/large").unwrap(), big(40000, 1));
            assert_eq!(read(fs, "home/again").unwrap(), big(40000, 1));
            assert_eq!(fs.metadata("home/large").unwrap().ino, fs.metadata("home/again").unwrap().ino);
            assert_eq!(read(fs, "etc/seed").unwrap(), b"changed");
            assert_eq!(read(fs, "etc/big").unwrap(), vec![5u8; 3000]);
            assert_eq!(fs.metadata("usr/bin/tool").unwrap().mode & 0o777, 0o755);
            assert!(fs.metadata_nofollow("usr/link").is_err());
            assert!(fs.metadata("var/old").is_err());
            assert_eq!(fs.read_link("home/sym").unwrap(), "large");
            assert!(read(fs, "tmp/scratch").is_none());
        };
        let fs = boot();
        assert_eq!(start(&fs, vec!["tmp".into()]), Found::Restored);
        check1(&fs);
        // Nothing changed: no generation.
        sync().unwrap();
        assert_eq!(STATE.lock().unwrap().as_ref().unwrap().sb.gen, g1);

        // A segment: one more file, one removed, one grown.
        write(&fs, "home/later", b"later");
        fs.unlink("home/small").unwrap();
        let h = fs.open("home/large", O_RDWR, 0).unwrap();
        h.write_at(40000, &big(9000, 2)).unwrap();
        drop(h);
        sync().unwrap();
        let st = STATE.lock().unwrap();
        let sb = st.as_ref().unwrap().sb;
        assert_eq!(sb.gen, g1 + 1);
        assert!(sb.jb > 0, "a segment, not a checkpoint");
        drop(st);
        let fs = boot();
        assert_eq!(start(&fs, vec!["tmp".into()]), Found::Restored);
        assert_eq!(read(&fs, "home/later").unwrap(), b"later");
        assert!(read(&fs, "home/small").is_none());
        assert_eq!(read(&fs, "home/large").unwrap().len(), 49000);

        // The last segment torn: the generation before it.
        {
            let mut f = STORE_FILE.lock().unwrap();
            let at = (sb.head as u64 - 1) * BS + HDR as u64;
            f[at as usize] ^= 0xff;
        }
        let fs = boot();
        assert_eq!(start(&fs, vec!["tmp".into()]), Found::FellBack);
        assert_eq!(STATE.lock().unwrap().as_ref().unwrap().sb.gen, g1);
        check1(&fs);
        // And writing goes on from there.
        write(&fs, "home/after", b"after");
        sync().unwrap();
        let fs = boot();
        assert_eq!(start(&fs, vec!["tmp".into()]), Found::Restored);
        assert_eq!(read(&fs, "home/after").unwrap(), b"after");
        assert_eq!(read(&fs, "home/small").unwrap(), b"small file");

        // The newest superblock torn: the one before.
        let gen = STATE.lock().unwrap().as_ref().unwrap().sb.gen;
        {
            let mut f = STORE_FILE.lock().unwrap();
            let at = ((gen % 2) * (BS / 2) + 20) as usize;
            f[at] ^= 0xff;
        }
        let fs = boot();
        assert_eq!(start(&fs, vec!["tmp".into()]), Found::FellBack);
        assert_eq!(STATE.lock().unwrap().as_ref().unwrap().sb.gen, gen - 1);
        check1(&fs);
        assert!(read(&fs, "home/after").is_none());
        reset();
    }

    /// Extended attributes set on a file of the sources (a program's
    /// `user.gex.mem64` record) are kept across boots without its bytes:
    /// the next boot's sources, newer bytes, keep them, as does a written
    /// file; a removed attribute stays removed.
    #[test]
    #[ignore]
    fn attributes_on_the_sources_files_are_kept_without_their_bytes() {
        use crate::vfs::XattrOp;
        STORE_FILE.lock().unwrap().clear();
        let get = |fs: &TmpFs, p: &str| {
            let mut b = [0u8; 8];
            fs.xattr(p, XattrOp::Get { name: "user.gex.mem64", buf: &mut b }).ok().map(|n| b[..n].to_vec())
        };
        let set = |fs: &TmpFs, p: &str| fs.xattr(p, XattrOp::Set { name: "user.gex.mem64", value: b"1", flags: 0 }).unwrap();
        let fs = boot();
        start(&fs, vec![]);
        set(&fs, "usr/bin/tool");
        set(&fs, "etc/big");
        write(&fs, "home-file", b"mine");
        set(&fs, "home-file");
        // (the write-back's tick, not only an explicit sync, sees it)
        let seen = STATE.lock().unwrap().as_ref().unwrap().seen;
        assert_ne!(META.load(Ordering::Relaxed), seen, "setting an attribute is a change to keep");
        sync().unwrap();

        // The next boot's sources: a newer tool.
        let fs = boot();
        fs.add_file("usr/bin/tool", b"#!v2".to_vec());
        assert_eq!(start(&fs, vec![]), Found::Restored);
        assert_eq!(get(&fs, "usr/bin/tool").as_deref(), Some(&b"1"[..]));
        assert_eq!(read(&fs, "usr/bin/tool").unwrap(), b"#!v2", "the sources' bytes, not a kept copy");
        assert_eq!(get(&fs, "etc/big").as_deref(), Some(&b"1"[..]));
        assert_eq!(read(&fs, "etc/big").unwrap(), vec![5u8; 3000]);
        assert_eq!(get(&fs, "home-file").as_deref(), Some(&b"1"[..]));
        assert_eq!(read(&fs, "home-file").unwrap(), b"mine");

        // Removed, and kept removed.
        fs.xattr("usr/bin/tool", XattrOp::Remove { name: "user.gex.mem64" }).unwrap();
        sync().unwrap();
        let fs = boot();
        assert_eq!(start(&fs, vec![]), Found::Restored);
        assert_eq!(get(&fs, "usr/bin/tool"), None);
        assert_eq!(get(&fs, "etc/big").as_deref(), Some(&b"1"[..]));
        reset();
    }

    /// A layer laid after boot (a site's tar added once the system runs):
    /// what was set on its files is kept, without their bytes, through a
    /// sync made before the layer is laid again and one made after.
    #[test]
    #[ignore]
    fn attributes_on_a_late_layer_are_kept_until_it_is_laid_again() {
        use crate::vfs::XattrOp;
        STORE_FILE.lock().unwrap().clear();
        let get = |fs: &TmpFs, p: &str| {
            let mut b = [0u8; 8];
            fs.xattr(p, XattrOp::Get { name: "user.gex.mem64", buf: &mut b }).ok().map(|n| b[..n].to_vec())
        };
        let fs = boot();
        start(&fs, vec![]);
        fs.mkdir("opt", 0o755).unwrap();
        fs.add_file("opt/app", b"layer-1".to_vec());
        fs.xattr("opt/app", XattrOp::Set { name: "user.gex.mem64", value: b"1", flags: 0 }).unwrap();
        sync().unwrap();

        // Booted, the layer not laid yet: a sync keeps the record.
        let fs = boot();
        assert_eq!(start(&fs, vec![]), Found::Restored);
        assert_eq!(get(&fs, "opt/app"), None);
        write(&fs, "home-file", b"mine");
        sync().unwrap();

        // Booted again, the layer (newer) laid after.
        let fs = boot();
        assert_eq!(start(&fs, vec![]), Found::Restored);
        fs.mkdir("opt", 0o755).ok();
        fs.add_file("opt/app", b"layer-2".to_vec());
        assert_eq!(get(&fs, "opt/app").as_deref(), Some(&b"1"[..]));
        assert_eq!(read(&fs, "opt/app").unwrap(), b"layer-2", "the layer's bytes, not a kept copy");
        sync().unwrap();
        let fs = boot();
        assert_eq!(start(&fs, vec![]), Found::Restored);
        fs.mkdir("opt", 0o755).ok();
        fs.add_file("opt/app", b"layer-3".to_vec());
        assert_eq!(get(&fs, "opt/app").as_deref(), Some(&b"1"[..]));
        assert_eq!(read(&fs, "opt/app").unwrap(), b"layer-3");
        reset();
    }

    /// Many generations fold into a checkpoint, and the old records' blocks
    /// are used again rather than the store growing.
    #[test]
    #[ignore]
    fn the_journal_folds_into_a_checkpoint() {
        STORE_FILE.lock().unwrap().clear();
        let fs = boot();
        start(&fs, vec![]);
        fs.mkdir("d", 0o755).unwrap();
        for i in 0..400 {
            write(&fs, &format!("d/f{}", i % 50), &big(300, i as u8));
            sync().unwrap();
        }
        assert!(CHECKPOINTS.load(Ordering::Relaxed) >= 1);
        let high = blocks::high();
        assert!(high < 200, "the store stays small: {high} blocks");
        let fs = boot();
        assert_eq!(start(&fs, vec![]), Found::Restored);
        for i in 350..400 {
            assert_eq!(read(&fs, &format!("d/f{}", i % 50)).unwrap(), big(300, i as u8));
        }
        reset();
    }
}
