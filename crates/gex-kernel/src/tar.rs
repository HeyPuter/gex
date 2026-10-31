//! Reading a tar: what GNU tar and libarchive write for a directory tree.
//! ustar (with its `prefix` field), GNU long names and link targets, GNU
//! base-256 sizes, and pax extended headers' path, link target and user
//! extended attributes (GNU's `SCHILY.xattr.user.*`, raw; libarchive's
//! `LIBARCHIVE.xattr.user.*`, base64).
//!
//! Two ways in over one header reader: [`entries`], a whole tar in memory
//! (the eject reads bundles so), and [`Reader`], a tar fed in parts with the
//! files' bytes passed on as they come (the page's trees into the root).

const BLOCK: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    /// A hard link: its `link` is a path from the archive's top, not from
    /// the link's directory.
    Hard,
    /// A device, a fifo, or anything else: skipped.
    Other,
}

#[derive(Clone, Debug)]
pub struct Header {
    /// The path, `.`, `..` and repeated slashes resolved, with no leading or
    /// trailing slash: empty for the archive's own top (`./`).
    pub path: String,
    pub kind: Kind,
    pub mode: u32,
    pub mtime: i64,
    pub size: u64,
    /// A symbolic link's target as written; a hard link's, resolved as
    /// `path` is.
    pub link: String,
    /// User extended attributes: `(user.NAME, value)`.
    pub xattrs: Vec<(String, Vec<u8>)>,
}

/// `p` with `.`, `..` and empty parts resolved, no leading or trailing slash.
pub fn clean(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out.join("/")
}

fn text(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn number(b: &[u8]) -> u64 {
    // GNU writes what octal cannot hold in base 256, the high bit set.
    if b[0] & 0x80 != 0 {
        return b[1..].iter().fold(0u64, |n, &c| (n << 8) | c as u64);
    }
    u64::from_str_radix(text(b).trim(), 8).unwrap_or(0)
}

fn base64(s: &[u8]) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let (mut out, mut acc, mut bits) = (Vec::new(), 0u32, 0);
    for v in s.iter().filter_map(|&c| val(c)) {
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// `%XX` decoded: libarchive's attribute names.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What a long-name or pax entry says of the entry after it.
#[derive(Default)]
struct Carry {
    name: Option<String>,
    link: Option<String>,
    pax: Option<Pax>,
}

#[derive(Default)]
struct Pax {
    path: Option<String>,
    link: Option<String>,
    xattrs: Vec<(String, Vec<u8>)>,
}

/// A pax header's records: `LEN key=value\n`, LEN counting the whole record.
fn pax(rec: &[u8]) -> Pax {
    let mut out = Pax::default();
    let mut at = 0;
    while at < rec.len() {
        let Some(sp) = rec[at..].iter().position(|&c| c == b' ') else { break };
        let Ok(len) = std::str::from_utf8(&rec[at..at + sp]).unwrap_or("").parse::<usize>() else { break };
        if len == 0 || at + len > rec.len() || sp + 1 >= len {
            break;
        }
        let kv = &rec[at + sp + 1..at + len - 1];
        at += len;
        let Some(eq) = kv.iter().position(|&c| c == b'=') else { continue };
        let (key, value) = (String::from_utf8_lossy(&kv[..eq]), &kv[eq + 1..]);
        if key == "path" {
            out.path = Some(String::from_utf8_lossy(value).into_owned());
        } else if key == "linkpath" {
            out.link = Some(String::from_utf8_lossy(value).into_owned());
        } else if let Some(n) = key.strip_prefix("SCHILY.xattr.").filter(|n| n.starts_with("user.")) {
            out.xattrs.push((n.to_string(), value.to_vec()));
        } else if let Some(n) = key.strip_prefix("LIBARCHIVE.xattr.").map(unescape).filter(|n| n.starts_with("user.")) {
            out.xattrs.push((n, base64(value)));
        }
    }
    out
}

/// One header block, read.
enum Block {
    /// Zeros: the end, or padding before it.
    Zero,
    /// A long name, a long link target or a pax header: its body (`size`
    /// bytes) is for the entry after it.
    Meta(u8, u64),
    Entry(Header),
}

fn block(h: &[u8], carry: &mut Carry) -> Block {
    if h.iter().all(|&c| c == 0) {
        return Block::Zero;
    }
    let size = number(&h[124..136]);
    let kind = h[156];
    if matches!(kind, b'L' | b'K' | b'x' | b'g') {
        return Block::Meta(kind, size);
    }
    let mut name = text(&h[0..100]);
    let prefix = text(&h[345..500]);
    if !prefix.is_empty() {
        name = format!("{prefix}/{name}");
    }
    let mut link = text(&h[157..257]);
    if let Some(n) = carry.name.take() {
        name = n;
    }
    if let Some(l) = carry.link.take() {
        link = l;
    }
    let mut xattrs = Vec::new();
    if let Some(p) = carry.pax.take() {
        name = p.path.unwrap_or(name);
        link = p.link.unwrap_or(link);
        xattrs = p.xattrs;
    }
    let kind = match kind {
        b'0' | 0 | b'7' => Kind::File,
        b'5' => Kind::Dir,
        b'2' => Kind::Symlink,
        b'1' => Kind::Hard,
        _ => Kind::Other,
    };
    if kind == Kind::Hard {
        link = clean(&link);
    }
    Block::Entry(Header {
        path: clean(&name),
        kind,
        mode: number(&h[100..108]) as u32 & 0o7777,
        mtime: number(&h[136..148]) as i64,
        size: if kind == Kind::File { size } else { 0 },
        link,
        xattrs,
    })
}

fn meta(kind: u8, body: &[u8], carry: &mut Carry) {
    match kind {
        b'L' => carry.name = Some(text(body)),
        b'K' => carry.link = Some(text(body)),
        b'x' => carry.pax = Some(pax(body)),
        _ => {}
    }
}

const fn padded(n: u64) -> u64 {
    n.div_ceil(BLOCK as u64) * BLOCK as u64
}

/// Every entry of the tar `data`, with where its bytes start in it.
pub fn entries(data: &[u8]) -> Result<Vec<(Header, usize)>, String> {
    let mut out = Vec::new();
    let mut carry = Carry::default();
    let mut off = 0usize;
    while off + BLOCK <= data.len() {
        let b = block(&data[off..off + BLOCK], &mut carry);
        let size = match &b {
            Block::Zero => 0,
            Block::Meta(_, size) => *size,
            // A link's or a directory's size, if it says one, is skipped.
            Block::Entry(_) => number(&data[off + 124..off + 136]),
        } as usize;
        let body = off + BLOCK;
        let next = body + padded(size as u64) as usize;
        if next > data.len() {
            return Err(format!("a tar entry runs past the end ({off:#x})"));
        }
        match b {
            Block::Zero => {}
            Block::Meta(kind, _) => meta(kind, &data[body..body + size], &mut carry),
            Block::Entry(h) => out.push((h, body)),
        }
        off = next;
    }
    Ok(out)
}

/// What [`Reader::feed`] passes on: an entry, then a file's bytes in order.
pub enum Event<'a> {
    Entry(Header),
    Data(&'a [u8]),
}

/// A tar read as it comes, in parts of any size.
#[derive(Default)]
pub struct Reader {
    /// A header block, and a meta entry's body, gathered across parts.
    buf: Vec<u8>,
    /// A meta entry being gathered: its kind and size.
    gathering: Option<(u8, u64)>,
    carry: Carry,
    /// What is left of a file's bytes, passed on, then of its padding (or
    /// another entry's body), skipped.
    data: u64,
    skip: u64,
}

impl Reader {
    pub fn feed(&mut self, mut part: &[u8], f: &mut dyn FnMut(Event)) {
        let take = |part: &mut &[u8], n: u64| -> usize {
            let n = (n.min(part.len() as u64)) as usize;
            *part = &part[n..];
            n
        };
        while !part.is_empty() {
            if self.data > 0 {
                let n = (self.data.min(part.len() as u64)) as usize;
                f(Event::Data(&part[..n]));
                self.data -= n as u64;
                part = &part[n..];
                continue;
            }
            if self.skip > 0 {
                self.skip -= take(&mut part, self.skip) as u64;
                continue;
            }
            let want = match self.gathering {
                Some((_, size)) => BLOCK + padded(size) as usize,
                None => BLOCK,
            };
            let n = (want - self.buf.len()).min(part.len());
            self.buf.extend_from_slice(&part[..n]);
            part = &part[n..];
            if self.buf.len() < want {
                return;
            }
            if let Some((kind, size)) = self.gathering.take() {
                let body = self.buf[BLOCK..BLOCK + size as usize].to_vec();
                meta(kind, &body, &mut self.carry);
                self.buf.clear();
                continue;
            }
            let size = number(&self.buf[124..136]);
            match block(&self.buf, &mut self.carry) {
                Block::Zero => {}
                Block::Meta(kind, size) => {
                    self.gathering = Some((kind, size));
                    continue;
                }
                Block::Entry(h) => {
                    if h.kind == Kind::File {
                        self.data = size;
                        self.skip = padded(size) - size;
                    } else {
                        self.skip = padded(size);
                    }
                    f(Event::Entry(h));
                }
            }
            self.buf.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(name: &str, kind: u8, size: usize, link: &str) -> Vec<u8> {
        let mut h = vec![0u8; BLOCK];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..107].copy_from_slice(b"0000755");
        h[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
        h[136..147].copy_from_slice(b"00000001750");
        h[156] = kind;
        h[157..157 + link.len()].copy_from_slice(link.as_bytes());
        h
    }

    fn entry(t: &mut Vec<u8>, name: &str, kind: u8, body: &[u8], link: &str) {
        t.extend(head(name, kind, body.len(), link));
        t.extend_from_slice(body);
        t.resize(t.len().div_ceil(BLOCK) * BLOCK, 0);
    }

    fn sample() -> Vec<u8> {
        let mut t = Vec::new();
        entry(&mut t, "./", b'5', b"", "");
        entry(&mut t, "./usr/bin/", b'5', b"", "");
        let long = format!("./usr/bin/{}", "n".repeat(150));
        entry(&mut t, "././@LongLink", b'L', format!("{long}\0").as_bytes(), "");
        entry(&mut t, "cut", b'0', &[7u8; 700], "");
        let rec = |k: &str, v: &str| {
            let body = format!(" {k}={v}\n");
            let mut n = body.len();
            while n.to_string().len() + body.len() != n {
                n = n.to_string().len() + body.len();
            }
            format!("{n}{body}")
        };
        let pax = format!("{}{}", rec("path", "usr/bin/sh"), rec("SCHILY.xattr.user.rt", "kept"));
        entry(&mut t, "PaxHeader", b'x', pax.as_bytes(), "");
        entry(&mut t, "cut2", b'0', b"#!x\n", "");
        entry(&mut t, "./bin", b'2', b"", "usr/bin");
        entry(&mut t, "./usr/bin/dash", b'1', b"", "./usr/bin/sh");
        t.extend(vec![0u8; 2 * BLOCK]);
        t
    }

    #[test]
    fn whole() {
        let t = sample();
        let e = entries(&t).unwrap();
        let names: Vec<_> = e.iter().map(|(h, _)| (h.path.as_str(), h.kind)).collect();
        assert_eq!(names[0], ("", Kind::Dir));
        assert_eq!(names[2], (format!("usr/bin/{}", "n".repeat(150)).as_str(), Kind::File));
        assert_eq!(names[3], ("usr/bin/sh", Kind::File));
        assert_eq!(e[3].0.xattrs, vec![("user.rt".to_string(), b"kept".to_vec())]);
        assert_eq!(&t[e[3].1..e[3].1 + 4], b"#!x\n");
        assert_eq!((e[4].0.kind, e[4].0.link.as_str()), (Kind::Symlink, "usr/bin"));
        assert_eq!((e[5].0.kind, e[5].0.link.as_str()), (Kind::Hard, "usr/bin/sh"));
        assert_eq!((e[2].0.mode, e[2].0.mtime, e[2].0.size), (0o755, 1000, 700));
    }

    #[test]
    fn in_parts() {
        let t = sample();
        for part in [1, 7, 511, 512, 513, 4096, t.len()] {
            let mut r = Reader::default();
            let mut got: Vec<(String, Vec<u8>)> = Vec::new();
            for c in t.chunks(part) {
                r.feed(c, &mut |e| match e {
                    Event::Entry(h) => got.push((h.path, Vec::new())),
                    Event::Data(d) => got.last_mut().unwrap().1.extend_from_slice(d),
                });
            }
            let whole: Vec<_> = entries(&t).unwrap().into_iter().map(|(h, at)| (h.path, t[at..at + h.size as usize].to_vec())).collect();
            assert_eq!(got, whole, "in parts of {part}");
        }
    }

    #[test]
    fn libarchive_names() {
        assert_eq!(unescape("user.a%20b"), "user.a b");
        assert_eq!(base64(b"a2VwdA=="), b"kept");
    }
}
