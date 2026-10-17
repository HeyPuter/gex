//! Resource trimming (`"resources": "trim"` in a W3 project's
//! user/eject.json): the bytes of each PE's resources that no lookup the
//! site's Wine makes can return are zeroed in the bundle, so the download
//! carries nothing for them. No size or offset in a file changes; a
//! resource directory only loses entries, rewritten in place.
//!
//! What can be returned is decided by the site, which fixes the locale and
//! the DPI (its page environment and its prefix's registry):
//!
//! * **Languages.** Wine's `find_entry` (dlls/ntdll/resource.c) looks a
//!   resource's language up in a list made from the request, the neutral
//!   language, the thread, user and system locales and English, and only
//!   when none is there takes the first entry. With an English locale, an
//!   entry whose primary language is neither neutral nor English is never
//!   on that list: it goes, unless it is all a name has (the first-entry
//!   fallback), which stays. A program that asked for another language by
//!   number would now get not-found where it got that language.
//! * **Ribbon images.** A ribbon's compiled markup (a `UIFILE` resource)
//!   names each command's images as records `[05|06] id:u32 MinDPI:u16`, one
//!   run per command and size, and the ribbon takes the record with the
//!   highest MinDPI not above the screen's. A bitmap named only by records
//!   the ribbon never takes at the site's DPI, whose id appears in no other
//!   byte of the markup and in no non-resource section of the image (no code
//!   or table that could load it by number), goes.
//! * **Bytes no directory reaches.** Whatever of a resource section the
//!   (rewritten) tree does not point at: entries dropped above, and copies
//!   left behind by a resource editor (Paint's merged MUI moved its whole
//!   directory into `.rsrcmui`, leaving `.rsrc` unread).
//!
//! An image is left whole when anything else points into what would be
//! zeroed: a relocated word, another data directory. Without base
//! relocations an image with code is left whole too, since an absolute
//! reference to its resources would then be invisible.

use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};

/// What the site's Wine looks resources up with.
#[derive(Clone, Debug)]
pub struct Site {
    /// The primary language (`PRIMARYLANGID`) every lookup's list holds
    /// besides the neutral one; `None`: languages are not trimmed.
    pub lang: Option<u16>,
    /// The screen's DPI (`LogPixels`); `None`: ribbon images are not trimmed.
    pub dpi: Option<u16>,
}

/// Bytes zeroed, by rule. `dead` is what the tree already did not reach
/// before any entry was dropped.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stat {
    pub languages: usize,
    pub dpi: usize,
    pub dead: usize,
    pub entries: usize,
}

const RT_BITMAP: u32 = 2;
const HIGH: u32 = 0x8000_0000;

struct Ent {
    name: u32,
    val: u32,
}

struct Dir {
    /// File offset of the directory header.
    at: usize,
    named: usize,
    ents: Vec<Ent>,
}

struct Tree<'a> {
    /// For its sections (rva to file offset).
    pe: &'a gex_ld::pe::PeImage,
    /// The file's bytes: the PE's, or a rewrite of them.
    data: &'a [u8],
    root: u32,
}

impl Tree<'_> {
    fn off(&self, rva: u32, n: usize) -> Option<usize> {
        let o = self.pe.rva_to_offset(rva)?;
        (o + n <= self.data.len()).then_some(o)
    }
    fn u16(&self, o: usize) -> u16 {
        u16::from_le_bytes([self.data[o], self.data[o + 1]])
    }
    fn u32(&self, o: usize) -> u32 {
        u32::from_le_bytes(self.data[o..o + 4].try_into().unwrap())
    }
    fn dir(&self, off: u32) -> Option<Dir> {
        let at = self.off(self.root.checked_add(off)?, 16)?;
        let named = self.u16(at + 12) as usize;
        let n = named + self.u16(at + 14) as usize;
        self.off(self.root + off, 16 + 8 * n)?;
        let ents = (0..n).map(|i| Ent { name: self.u32(at + 16 + 8 * i), val: self.u32(at + 20 + 8 * i) }).collect();
        Some(Dir { at, named, ents })
    }
    /// A named entry's name (`IMAGE_RESOURCE_DIR_STRING_U`).
    fn name(&self, e: &Ent) -> Option<String> {
        if e.name & HIGH == 0 {
            return None;
        }
        let o = self.off(self.root + (e.name & !HIGH), 2)?;
        let n = self.u16(o) as usize;
        let o = self.off(self.root + (e.name & !HIGH), 2 + 2 * n)?;
        let w: Vec<u16> = (0..n).map(|i| self.u16(o + 2 + 2 * i)).collect();
        Some(String::from_utf16_lossy(&w))
    }
    /// A leaf's data: (rva, size).
    fn data(&self, e: &Ent) -> Option<(u32, u32)> {
        if e.val & HIGH != 0 {
            return None;
        }
        let o = self.off(self.root + e.val, 16)?;
        Some((self.u32(o), self.u32(o + 4)))
    }
}

/// One resource: type, name and language entries as they sit in the tree.
struct Res {
    ty: u32,
    ty_name: Option<String>,
    name: u32,
    /// The language directory.
    langs: Dir,
}

fn walk(t: &Tree) -> Option<(Dir, Vec<(Dir, Vec<Res>)>)> {
    let root = t.dir(0)?;
    let mut types = Vec::new();
    for te in &root.ents {
        if te.val & HIGH == 0 {
            return None;
        }
        let nd = t.dir(te.val & !HIGH)?;
        let mut res = Vec::new();
        for ne in &nd.ents {
            if ne.val & HIGH == 0 {
                return None;
            }
            let langs = t.dir(ne.val & !HIGH)?;
            if langs.ents.iter().any(|l| l.val & HIGH != 0) {
                return None;
            }
            res.push(Res { ty: te.name, ty_name: t.name(te), name: ne.name, langs });
        }
        types.push((nd, res));
    }
    Some((root, types))
}

/// The byte ranges (rva, len) the tree under `t` reaches.
fn reached(t: &Tree) -> Option<Vec<(u32, u32)>> {
    let mut out = Vec::new();
    let mut stack = vec![0u32];
    let mut seen = HashSet::new();
    while let Some(off) = stack.pop() {
        if !seen.insert(off) {
            continue;
        }
        let d = t.dir(off)?;
        out.push((t.root + off, 16 + 8 * d.ents.len() as u32));
        for e in &d.ents {
            if e.name & HIGH != 0 {
                let o = t.off(t.root + (e.name & !HIGH), 2)?;
                out.push((t.root + (e.name & !HIGH), 2 + 2 * t.u16(o) as u32));
            }
            if e.val & HIGH != 0 {
                stack.push(e.val & !HIGH);
            } else {
                out.push((t.root + e.val, 16));
                let (rva, size) = t.data(e)?;
                out.push((rva, size));
            }
        }
    }
    Some(out)
}

/// The ribbon bitmaps no lookup at `dpi` takes (module docs).
fn ribbon_unused(t: &Tree, types: &[(Dir, Vec<Res>)], dpi: u16) -> HashSet<u32> {
    let bitmaps: HashSet<u32> = types.iter().flat_map(|(_, r)| r).filter(|r| r.ty == RT_BITMAP).map(|r| r.name).collect();
    let mut markup: Vec<&[u8]> = Vec::new();
    for r in types.iter().flat_map(|(_, r)| r).filter(|r| r.ty_name.as_deref() == Some("UIFILE")) {
        for l in &r.langs.ents {
            if let Some((rva, size)) = t.data(l) {
                if let Some(o) = t.off(rva, size as usize) {
                    markup.push(&t.data[o..o + size as usize]);
                }
            }
        }
    }
    if markup.is_empty() {
        return HashSet::new();
    }
    let dpis = [96u16, 120, 144, 192];
    let mut taken: HashSet<u32> = HashSet::new();
    let mut named: HashMap<u32, usize> = HashMap::new();
    for m in &markup {
        let rec = |i: usize| -> Option<(u8, u32, u16)> {
            let b = m.get(i..i + 7)?;
            let (tag, id, d) = (b[0], u32::from_le_bytes(b[1..5].try_into().unwrap()), u16::from_le_bytes([b[5], b[6]]));
            ((tag == 5 || tag == 6) && bitmaps.contains(&id) && dpis.contains(&d)).then_some((tag, id, d))
        };
        let mut i = 0;
        while i < m.len() {
            let Some((tag, _, _)) = rec(i) else {
                i += 1;
                continue;
            };
            // A run: one command's images of one size, a record per MinDPI.
            let mut run = Vec::new();
            while let Some(r) = rec(i).filter(|r| r.0 == tag) {
                run.push(r);
                i += 7;
            }
            for r in &run {
                *named.entry(r.1).or_default() += 1;
            }
            match run.iter().filter(|r| r.2 <= dpi).max_by_key(|r| r.2) {
                Some(r) => {
                    taken.insert(r.1);
                }
                None => taken.extend(run.iter().map(|r| r.1)),
            }
        }
    }
    // Every other mention: the markup's bytes, and every section that is not
    // the resources (code that loads a bitmap by number, a table of ids).
    let mut rest: Vec<&[u8]> = markup.clone();
    for s in &t.pe.sections {
        if is_resource_section(t, s) {
            continue;
        }
        let o = s.raw_offset as usize;
        if let Some(b) = t.data.get(o..o + s.raw_size as usize) {
            rest.push(b);
        }
    }
    let mut seen: HashMap<u32, usize> = named.keys().filter(|id| !taken.contains(id)).map(|&id| (id, 0)).collect();
    for b in &rest {
        for w in b.windows(4) {
            if let Some(n) = seen.get_mut(&u32::from_le_bytes(w.try_into().unwrap())) {
                *n += 1;
            }
        }
    }
    // The markup's own records are the mentions expected.
    seen.into_iter().filter(|(id, n)| named.get(id) == Some(n)).map(|(id, _)| id).collect()
}

fn is_resource_section(t: &Tree, s: &gex_ld::pe::Section) -> bool {
    let span = s.vsize.max(s.raw_size);
    s.name.starts_with(".rsrc") || (t.root >= s.rva && t.root - s.rva < span)
}

/// `file` with its unreachable resources zeroed, or `None` when it is not a
/// PE with resources or nothing changes (module docs).
pub fn trim(file: &[u8], site: &Site) -> Option<(Vec<u8>, Stat)> {
    if !file.starts_with(b"MZ") {
        return None;
    }
    let pe = gex_ld::pe::parse(file).ok()?;
    let (root, size) = *pe.dirs.get(2)?;
    if root == 0 || size == 0 {
        return None;
    }
    let has_code = pe.sections.iter().any(|s| s.executable());
    if has_code && pe.relocs.is_empty() {
        return None;
    }
    let t = Tree { pe: &pe, data: file, root };
    let before: Vec<(u32, u32)> = reached(&t)?;
    let (rootdir, types) = walk(&t)?;
    let unused = site.dpi.map(|d| ribbon_unused(&t, &types, d)).unwrap_or_default();

    let mut out = file.to_vec();
    let mut st = Stat::default();
    let mut dropped_lang: Vec<(u32, u32)> = Vec::new();
    let mut dropped_dpi: Vec<(u32, u32)> = Vec::new();
    // Rewrite a directory with only `keep` of its entries, in order.
    let rewrite = |out: &mut Vec<u8>, d: &Dir, keep: &[bool]| {
        let kept: Vec<&Ent> = d.ents.iter().zip(keep).filter(|(_, k)| **k).map(|(e, _)| e).collect();
        let named = d.ents.iter().zip(keep).take(d.named).filter(|(_, k)| **k).count();
        out[d.at + 12..d.at + 14].copy_from_slice(&(named as u16).to_le_bytes());
        out[d.at + 14..d.at + 16].copy_from_slice(&((kept.len() - named) as u16).to_le_bytes());
        for (i, e) in kept.iter().enumerate() {
            let o = d.at + 16 + 8 * i;
            out[o..o + 4].copy_from_slice(&e.name.to_le_bytes());
            out[o + 4..o + 8].copy_from_slice(&e.val.to_le_bytes());
        }
        for o in d.at + 16 + 8 * kept.len()..d.at + 16 + 8 * d.ents.len() {
            out[o] = 0;
        }
    };
    let mut type_keep = Vec::new();
    for (nd, res) in &types {
        let mut name_keep = Vec::new();
        for r in res {
            let gone = r.ty == RT_BITMAP && r.name & HIGH == 0 && unused.contains(&r.name);
            let mut keep: Vec<bool> = r
                .langs
                .ents
                .iter()
                .map(|l| {
                    if gone {
                        return false;
                    }
                    let Some(want) = site.lang else { return true };
                    // A named language is nothing Wine's list holds; keep it.
                    let primary = (l.name & 0x3ff) as u16;
                    l.name & HIGH != 0 || primary == 0 || primary == want
                })
                .collect();
            // Wine's last resort is the first entry: a name with nothing on
            // the list keeps it.
            if !gone && !keep.iter().any(|k| *k) && !keep.is_empty() {
                keep[0] = true;
            }
            for (l, k) in r.langs.ents.iter().zip(&keep) {
                if !k {
                    if let Some(d) = t.data(l) {
                        if gone { dropped_dpi.push(d) } else { dropped_lang.push(d) }
                    }
                    st.entries += 1;
                }
            }
            if keep.iter().any(|k| !k) {
                rewrite(&mut out, &r.langs, &keep);
            }
            name_keep.push(keep.iter().any(|k| *k));
        }
        if name_keep.iter().any(|k| !k) {
            rewrite(&mut out, nd, &name_keep);
        }
        type_keep.push(name_keep.iter().any(|k| *k));
    }
    if type_keep.iter().any(|k| !k) {
        rewrite(&mut out, &rootdir, &type_keep);
    }

    // What the rewritten tree reaches: read it back from `out`.
    let after = reached(&Tree { pe: &pe, data: &out, root })?;
    let secs: Vec<&gex_ld::pe::Section> = pe.sections.iter().filter(|s| is_resource_section(&t, s)).collect();
    // The rvas that must survive: relocated words' targets, other directories.
    let ptr = if pe.is64 { 8 } else { 4 };
    let mut pinned: Vec<(u32, u32)> = Vec::new();
    for r in &pe.relocs {
        if let Some(o) = pe.rva_to_offset(r.rva) {
            if let Some(b) = file.get(o..o + ptr) {
                let v = if ptr == 8 { u64::from_le_bytes(b.try_into().unwrap()) } else { u32::from_le_bytes(b.try_into().unwrap()) as u64 };
                if let Some(rva) = v.checked_sub(pe.preferred_base) {
                    pinned.push((rva as u32, 1));
                }
            }
        }
        pinned.push((r.rva, ptr as u32));
    }
    for (i, &(rva, n)) in pe.dirs.iter().enumerate() {
        // 4: the certificate table's "rva" is a file offset, outside every section.
        if i != 2 && i != 4 && rva != 0 {
            pinned.push((rva, n.max(1)));
        }
    }
    let mut zeroed_before_drops = 0usize;
    for s in &secs {
        let len = s.raw_size as usize;
        let mut live = vec![false; len];
        let mut was = vec![false; len];
        let mark = |m: &mut Vec<bool>, ranges: &[(u32, u32)]| {
            for &(rva, n) in ranges {
                let lo = rva.max(s.rva);
                let hi = (rva as u64 + n as u64).min(s.rva as u64 + len as u64) as u32;
                for i in lo..hi.max(lo) {
                    m[(i - s.rva) as usize] = true;
                }
            }
        };
        mark(&mut live, &after);
        mark(&mut was, &before);
        let mut pin = vec![false; len];
        mark(&mut pin, &pinned);
        if pin.iter().zip(&live).any(|(p, l)| *p && !*l) {
            // Something besides the tree points at bytes the trim would
            // clear: leave the whole image as it is.
            return None;
        }
        let o = s.raw_offset as usize;
        for i in 0..len {
            if !live[i] && out.get(o + i).is_some_and(|b| *b != 0) {
                out[o + i] = 0;
                if !was[i] {
                    zeroed_before_drops += 1;
                }
            }
        }
    }
    let count = |ranges: &[(u32, u32)]| ranges.iter().map(|r| r.1 as usize).sum::<usize>();
    st.dead = zeroed_before_drops;
    st.languages = count(&dropped_lang);
    st.dpi = count(&dropped_dpi);
    (out != file).then_some((out, st))
}

/// The site's locale and DPI, from the page's environment and the bundle's
/// registry (`prefix`: the guest path of WINEPREFIX).
pub fn site(env: &[String], tar: &crate::reach::Tar, prefix: &str) -> Site {
    // Wine takes the locale from the Unix one; none (or C/POSIX) is en-US.
    let var = |k: &str| env.iter().find_map(|e| e.strip_prefix(&format!("{k}="))).filter(|v| !v.is_empty());
    let loc = var("LC_ALL").or_else(|| var("LC_MESSAGES")).or_else(|| var("LANG"));
    let lang = match loc {
        None => Some(0x09),
        Some(v) if v == "C" || v == "POSIX" || v.starts_with("C.") || v.starts_with("en") => Some(0x09),
        Some(_) => None,
    };
    // Every LogPixels the prefix sets, and only when they agree.
    let mut dpis = HashSet::new();
    for f in ["user.reg", "system.reg"] {
        if let Some(b) = tar.read(&format!("{}/{f}", prefix.trim_end_matches('/'))) {
            for line in String::from_utf8_lossy(b).lines() {
                if let Some(v) = line.strip_prefix("\"LogPixels\"=dword:") {
                    dpis.insert(u32::from_str_radix(v.trim(), 16).unwrap_or(0));
                }
            }
        }
    }
    let dpi = (dpis.len() == 1).then(|| *dpis.iter().next().unwrap()).filter(|d| (72..=480).contains(d)).map(|d| d as u16);
    Site { lang, dpi }
}

/// The flavors a unit's key may name that are the page's, not this gexc's:
/// `unguarded` (mode bit 4, `gex_cgen_set_mode`) and W3's KUSER_SHARED_DATA
/// fold (`gexc cgen-image --w3-kuser`, which the runtime turns on for a W3
/// Wine that maps it low). [`key_with`] for each; callers keep the one that
/// gives the recorded key.
pub(crate) const FLAVORS: [(bool, bool); 4] = [(true, false), (false, false), (true, true), (false, true)];

/// The hash of `gex_cgen::image::abi_fingerprint` with W3's KUSER fold on
/// or off: the C header a unit is built against has the fold's helpers only
/// with it on (`gexc cgen-image --w3-kuser`), so the two flavors' keys differ
/// there as well as in their `:w3kuser`. Each is computed once, with the
/// process's fold set for the call and put back.
fn abi_hash(kuser: bool) -> String {
    static H: [std::sync::OnceLock<String>; 2] = [std::sync::OnceLock::new(), std::sync::OnceLock::new()];
    H[kuser as usize]
        .get_or_init(|| {
            let was = gex_abi::kuser_fold();
            gex_abi::set_kuser_fold(kuser);
            let mut h = gex_key::sha256::Sha256::new();
            h.update(gex_cgen::image::abi_fingerprint().as_bytes());
            gex_abi::set_kuser_fold(was);
            gex_key::sha256::hex(&h.finish())
        })
        .clone()
}

/// `gex_cgen::image::image_key` for the page's flavor (see [`FLAVORS`]). The
/// composition is `image::unit_inputs`'s, which [`rekey`] checks against a
/// recorded key for every image.
pub(crate) fn key_with(image: &gex_ld::Image, (unguarded, kuser): (bool, bool)) -> String {
    let mut inputs = format!("{}:{}", gex_cgen::image::UNIT_INPUTS, abi_hash(kuser));
    if unguarded {
        inputs.push_str(":unguarded");
    }
    if !gex_cgen::image::narrow_stores() {
        inputs.push_str(":widest");
    }
    if gex_cgen::image::icall_count() {
        inputs.push_str(":icallcount");
    }
    if kuser {
        inputs.push_str(":w3kuser");
    }
    if gex_abi::mem64() {
        inputs.push_str(":mem64");
    }
    gex_key::unit_key(image, gex_cgen::image::FORMAT, &inputs, gex_cgen::image::UNIT_ABI_VERSION, gex_cgen::image::RECOVERY_INPUTS)
}

/// A unit's key for its image trimmed: `None` unless this gexc computes
/// `old` for the untrimmed image at the base the unit was built for (so the
/// trimmed key is the one the page will compute) and every byte outside the
/// resources is unchanged. A unit key hashes the sources and the C header
/// of the gexc that made it, so this holds for a project built by its own
/// gexc (build.sh: `runtime/gexc eject build`).
pub fn rekey(unit_wasm: &[u8], orig: &[u8], trimmed: &[u8], old: &str) -> Result<Option<String>> {
    let pe = gex_ld::pe::parse(orig).map_err(|e| anyhow!("{e}"))?;
    let unit = gex_split::Unit::parse(unit_wasm)?;
    let addrs: HashSet<u64> = unit.addresses().into_iter().collect();
    let Some(base) = crate::reach::find_base(&pe, &addrs) else { return Ok(None) };
    let image = |pe: &gex_ld::pe::PeImage| -> Result<gex_ld::Image> {
        Ok(gex_xlate::func::pe_input(pe, base).map_err(|e| anyhow!("{e}"))?.0)
    };
    let was = image(&pe)?;
    if std::env::var_os("GEX_RSRC_TRACE").is_some() {
        eprintln!("[rsrc] {old}: base {base:#x} (preferred {:#x}); identity {}", pe.preferred_base, gex_key::image_identity(&was));
        for g in FLAVORS {
            eprintln!("[rsrc]   (unguarded, w3kuser)={g:?} -> {}", key_with(&was, g));
        }
    }
    let Some(g) = FLAVORS.into_iter().find(|&g| key_with(&was, g) == old) else { return Ok(None) };
    let t = Tree { pe: &pe, data: orig, root: pe.dirs.get(2).map_or(0, |d| d.0) };
    for s in pe.sections.iter().filter(|s| !is_resource_section(&t, s)) {
        let r = s.raw_offset as usize..(s.raw_offset + s.raw_size) as usize;
        if orig.get(r.clone()) != trimmed.get(r) {
            return Ok(None);
        }
    }
    let pe2 = gex_ld::pe::parse(trimmed).map_err(|e| anyhow!("{e}"))?;
    Ok(Some(key_with(&image(&pe2)?, g)))
}
