//! The reads the wineserver makes of a PE image when it maps it
//! (server/mapping.c `get_image_params`: the DOS and NT headers, the section
//! table, the export name of a Wine builtin or the version resource of any
//! other, the CLR and load-config headers, and the raw data of the shared
//! writable sections), so a client can have them fetched before its
//! `create_mapping` ([`super::FADV_GEX_IMAGE`]). The server makes those reads
//! inside a door call, under the door lock every process's server calls
//! take: a read of a lazy file there waited on the network with every
//! process's server calls waiting behind it (a 22 MB DLL's 16 MiB readahead
//! run was 24 s at 0.7 MB/s). Lazy files are fetched by the MiB, so what
//! matters is which chunks the reads touch: offsets here may be a few bytes
//! wide of the server's (its sector rounding), never a chunk.

/// `rd(offset, len)`: the bytes there (fewer at the end of the file), or
/// `None` when they cannot be had (the walk stops).
pub type Read<'a> = dyn FnMut(u64, usize) -> Option<Vec<u8>> + 'a;

const RT_VERSION: u32 = 16;
const SCN_MEM_SHARED: u32 = 0x1000_0000;
const SCN_MEM_WRITE: u32 = 0x8000_0000;
/// What one data directory read may be at most (a version resource is a few
/// KiB; this only bounds a corrupt size).
const MAX_DIR: usize = 1 << 20;

fn u16_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?) as u32)
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

#[derive(Clone, Copy)]
struct Section {
    va: u32,
    vsize: u32,
    raw: u32,
    raw_size: u32,
    flags: u32,
}

struct Image<'r, 'a> {
    rd: &'r mut Read<'a>,
    sec: Vec<Section>,
}

impl Image<'_, '_> {
    /// `load_data_dir`: `len` bytes at `va`, from the section holding it.
    fn at_va(&mut self, va: u32, len: usize) -> Option<Vec<u8>> {
        if va == 0 || len == 0 {
            return None;
        }
        let s = *self.sec.iter().find(|s| va >= s.va && va - s.va < s.vsize.max(s.raw_size))?;
        let off = (s.raw & !0x1ff) as u64 + (va - s.va) as u64;
        (self.rd)(off, len.min(MAX_DIR))
    }

    /// `find_resource_entry`: the entry `id` (a directory) of the directory
    /// at `offset` in the resource data, or for `id` 0 a language's
    /// (0x409, then neutral, then the first) data entry.
    fn resource_entry(&mut self, res: (u32, u32), offset: u32, id: u32) -> Option<u32> {
        let (rva, size) = res;
        if offset >= size {
            return None;
        }
        let dir = self.at_va(rva + offset, 16)?;
        let (named, ids) = (u16_at(&dir, 12)?, u16_at(&dir, 14)?);
        let first = offset + 16 + named * 8;
        if first >= size || ids == 0 {
            return None;
        }
        let e = self.at_va(rva + first, ids as usize * 8)?;
        let entries: Vec<(u32, u32)> = e.chunks_exact(8).map(|c| (u32_at(c, 0).unwrap() & 0xffff, u32_at(c, 4).unwrap())).collect();
        let find = |want: u32, dir: bool| {
            entries.iter().find(|(n, o)| *n == want && (o >> 31 == 1) == dir).map(|(_, o)| o & 0x7fff_ffff)
        };
        if id != 0 {
            return find(id, true);
        }
        find(0x409, false).or_else(|| find(0, false)).or_else(|| entries.first().filter(|(_, o)| o >> 31 == 0).map(|(_, o)| *o))
    }
}

/// Make the server's reads of the image through `rd`, in its order.
pub fn server_reads(rd: &mut Read<'_>) {
    let _ = walk(rd);
}

fn walk(rd: &mut Read<'_>) -> Option<()> {
    // The DOS header and the 32 bytes after it (a builtin's signature).
    let mz = rd(0, 96)?;
    if mz.get(..2)? != b"MZ" {
        return None;
    }
    let pos = u32_at(&mz, 0x3c)? as u64;
    // Signature, file header, the larger optional header.
    let nt = rd(pos, 4 + 20 + 240)?;
    if nt.get(..4)? != b"PE\0\0" {
        return None;
    }
    let nsec = u16_at(&nt, 4 + 2)? as usize;
    let opt_size = u16_at(&nt, 4 + 16)? as u64;
    let opt = 24;
    let (count_at, dirs_at) = match u16_at(&nt, opt)? {
        0x10b => (opt + 92, opt + 96),
        0x20b => (opt + 108, opt + 112),
        _ => return None,
    };
    let ndirs = u32_at(&nt, count_at).unwrap_or(0).min(16) as usize;
    let dir = |i: usize| -> Option<(u32, u32)> {
        if i >= ndirs {
            return None;
        }
        let (va, size) = (u32_at(&nt, dirs_at + i * 8)?, u32_at(&nt, dirs_at + i * 8 + 4)?);
        (va != 0 && size != 0).then_some((va, size))
    };
    let (exp, res, cfg, clr) = (dir(0), dir(2), dir(10), dir(14));
    let st = rd(pos + 24 + opt_size, nsec * 40)?;
    let sec = st
        .chunks_exact(40)
        .map(|c| Section { va: u32_at(c, 12).unwrap(), vsize: u32_at(c, 8).unwrap(), raw: u32_at(c, 20).unwrap(), raw_size: u32_at(c, 16).unwrap(), flags: u32_at(c, 36).unwrap() })
        .collect();
    let mut img = Image { rd, sec };
    // A Wine builtin's export name, any other image's version resource: the
    // server reads one; which one hangs on the builtin signature, and both
    // are small, so both.
    if let Some((va, size)) = exp {
        if let Some(e) = img.at_va(va, 40) {
            let name = u32_at(&e, 12).unwrap_or(0);
            if name > va && name < va.saturating_add(size) {
                img.at_va(name, 1024);
            }
        }
    }
    if let Some(r) = res {
        let entry = img.resource_entry(r, 0, RT_VERSION).and_then(|o| img.resource_entry(r, o, 1)).and_then(|o| img.resource_entry(r, o, 0));
        if let Some(o) = entry.filter(|o| *o < r.1) {
            if let Some(d) = img.at_va(r.0 + o, 16) {
                let (data, size) = (u32_at(&d, 0).unwrap_or(0), u32_at(&d, 4).unwrap_or(0));
                img.at_va(data, size as usize);
            }
        }
    }
    if let Some((va, size)) = clr {
        img.at_va(va, (size as usize).min(72));
    }
    if let Some((va, size)) = cfg {
        img.at_va(va, (size as usize).min(0x140));
    }
    // build_shared_mapping: the raw data of each shared writable section.
    for s in img.sec.clone() {
        if s.flags & SCN_MEM_SHARED != 0 && s.flags & SCN_MEM_WRITE != 0 && s.raw != 0 && s.raw_size != 0 {
            let off = (s.raw & !0x1ff) as u64;
            let mut at = 0usize;
            let len = s.raw_size as usize + (s.raw & 0x1ff) as usize;
            while at < len {
                let n = (len - at).min(MAX_DIR);
                (img.rd)(off + at as u64, n)?;
                at += n;
            }
        }
    }
    Some(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A PE32 image of `size` bytes: one `.text` section at file 0x400 (RVA
    /// 0x1000), a `.rsrc` at file offset `rsrc_at` (RVA 0x100000) holding a
    /// version resource whose data is `ver_at` bytes into it, and an export
    /// directory in `.text` naming the DLL.
    pub(crate) fn image(size: usize, rsrc_at: u32, ver_at: u32) -> Vec<u8> {
        let mut b = vec![0u8; size];
        let put16 = |b: &mut Vec<u8>, o: usize, v: u16| b[o..o + 2].copy_from_slice(&v.to_le_bytes());
        let put32 = |b: &mut Vec<u8>, o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
        b[0..2].copy_from_slice(b"MZ");
        put32(&mut b, 0x3c, 0x80);
        b[0x80..0x84].copy_from_slice(b"PE\0\0");
        put16(&mut b, 0x84 + 2, 2); // sections
        put16(&mut b, 0x84 + 16, 224); // optional header size
        let opt = 0x98;
        put16(&mut b, opt, 0x10b);
        put32(&mut b, opt + 92, 16);
        put32(&mut b, opt + 96, 0x1100); // export
        put32(&mut b, opt + 100, 0x100);
        put32(&mut b, opt + 96 + 16, 0x100000); // resource
        put32(&mut b, opt + 100 + 16, ver_at + 0x100);
        let st = opt + 224;
        // .text: VA 0x1000, vsize 0x1000, raw 0x400 size 0x1000
        put32(&mut b, st + 8, 0x1000);
        put32(&mut b, st + 12, 0x1000);
        put32(&mut b, st + 16, 0x1000);
        put32(&mut b, st + 20, 0x400);
        // .rsrc: VA 0x100000, raw rsrc_at
        put32(&mut b, st + 40 + 8, ver_at + 0x100);
        put32(&mut b, st + 40 + 12, 0x100000);
        put32(&mut b, st + 40 + 16, ver_at + 0x100);
        put32(&mut b, st + 40 + 20, rsrc_at);
        // export directory at RVA 0x1100 (file 0x500), its name at RVA 0x1180
        put32(&mut b, 0x500 + 12, 0x1180);
        b[0x580..0x588].copy_from_slice(b"test.dll");
        // resources: root -> 16 (dir at 0x18) -> 1 (dir at 0x30) -> 0x409 (data entry at 0x48)
        let r = rsrc_at as usize;
        let dir = |b: &mut Vec<u8>, o: usize, id: u32, to: u32| {
            put16(b, o + 14, 1);
            put32(b, o + 16, id);
            put32(b, o + 20, to);
        };
        dir(&mut b, r, 16, 0x8000_0018);
        dir(&mut b, r + 0x18, 1, 0x8000_0030);
        dir(&mut b, r + 0x30, 0x409, 0x48);
        put32(&mut b, r + 0x48, 0x100000 + ver_at);
        put32(&mut b, r + 0x48 + 4, 0x80);
        b[r + ver_at as usize..r + ver_at as usize + 4].copy_from_slice(b"VERS");
        b
    }

    fn reads(img: &[u8]) -> Vec<(u64, usize, Vec<u8>)> {
        let mut seen = Vec::new();
        server_reads(&mut |at, n| {
            let s = (at as usize).min(img.len());
            let e = (s + n).min(img.len());
            seen.push((at, n, img[s..e].to_vec()));
            Some(img[s..e].to_vec())
        });
        seen
    }

    #[test]
    fn walks_headers_export_name_and_version_resource() {
        let img = image(20 << 20, 0x40_0000, 0xe0_0000);
        let r = reads(&img);
        assert_eq!(r[0].0, 0);
        assert!(r.iter().any(|(_, _, b)| b.starts_with(b"test.dll")), "the export name");
        // the version data, 0xe00000 into .rsrc at 0x400000
        assert!(r.iter().any(|(at, _, b)| *at == 0x40_0000 + 0xe0_0000 && b.starts_with(b"VERS")), "{:x?}", r.iter().map(|x| x.0).collect::<Vec<_>>());
    }

    #[test]
    fn stops_at_a_file_that_is_no_image() {
        assert_eq!(reads(&vec![0u8; 4096]).len(), 1);
    }
}
