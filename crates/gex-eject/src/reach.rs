//! Static reachability of a unit's functions, for pruning (`"prune":
//! "static"`, docs/eject.md "Pruning"): which of an image's recovered
//! functions no path from outside the image can reach, so that a site can
//! leave their code out.
//!
//! The proof is over the image as recovery sees it (`gex_xlate::func`):
//!
//! * **Roots** are every function start with any source other than a direct
//!   call: the entry point, exports, TLS callbacks, PE base-relocation
//!   targets (in a PE32 with relocations every absolute address the image
//!   holds, in code or data, is one: vtables, callbacks, jump tables,
//!   handlers), untyped data and code pointers, import thunks, `.eh_frame`
//!   starts, landing pads. An address inside a function (a relocated label, a
//!   handler) makes every function whose body holds it a root.
//! * **The closure** follows, from each reachable function, its direct calls
//!   and tail calls, the continuation of every call (`ret_pc`, which a callee
//!   recovery takes not to return may still return to), a syscall's
//!   continuation, and the unit's own direct calls between slots (wasm
//!   `call`/`return_call`, followed through helpers).
//! * **Kept whole:** an image without base relocations (an absolute address
//!   could hide anywhere), with a CLR header (managed code calls by token),
//!   or with a get-PC idiom (`call $+5`: addresses computed from the return
//!   address). A slot recovery does not account for (no function or interior
//!   entry at its address) is kept, and so is everything it reaches.
//!
//! What is left out is only ever reached by a direct call from code that is
//! itself left out. A pruned slot's stub reports its address and traps
//! (gex_split::prune), so a wrong proof fails at once, by name.

use anyhow::{bail, Context, Result};
use gex_ir::func::{Callee, FTerm};
use gex_xlate::func::Source;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

/// A unit's reachable slots, and how the answer was reached.
pub struct Reach {
    /// Slots to keep.
    pub keep: HashSet<u32>,
    /// Why the whole unit is kept, when it is.
    pub whole: Option<String>,
    /// Where the image was placed (the unit's addresses).
    pub base: u64,
    /// Recovered functions, and how many of them are roots.
    pub functions: usize,
    pub roots: usize,
    /// Slots recovery does not account for (kept).
    pub unknown: usize,
}

/// A slot's address must be one of these for the proof to cover it.
const ROOT_SOURCES: [Source; 11] = [
    Source::Entry,
    Source::Symbol,
    Source::EhFrame,
    Source::InitArray,
    Source::Reloc,
    Source::DataPtr,
    Source::CodePtr,
    Source::LandingPad,
    Source::Orphan,
    Source::ImportThunk,
    Source::PeReloc,
];

fn whole(u: &gex_split::Unit, base: u64, why: impl Into<String>) -> Reach {
    Reach {
        keep: (0..u.slots.len() as u32).collect(),
        whole: Some(why.into()),
        base,
        functions: 0,
        roots: 0,
        unknown: 0,
    }
}

/// The base the unit's image was placed at: the 64 KiB-aligned base at which
/// most of the image's entry point and exports are slots.
fn find_base(pe: &gex_ld::pe::PeImage, addrs: &HashSet<u64>) -> Option<u64> {
    let mut rvas: Vec<u64> = pe
        .exports
        .entries
        .iter()
        .filter(|e| e.rva != 0 && e.forwarder.is_none())
        .map(|e| e.rva as u64)
        .filter(|&r| pe.sections.iter().any(|s| s.executable() && r >= s.rva as u64 && r < s.rva as u64 + s.vsize.max(s.raw_size) as u64))
        .collect();
    if pe.entry_rva != 0 {
        rvas.push(pe.entry_rva as u64);
    }
    rvas.sort_unstable();
    rvas.dedup();
    if rvas.is_empty() {
        return Some(pe.preferred_base);
    }
    let score = |b: u64| rvas.iter().filter(|&&r| addrs.contains(&(b + r))).count();
    let mut cands: HashSet<u64> = HashSet::from([pe.preferred_base]);
    for &r in rvas.iter().take(8) {
        for &a in addrs {
            if let Some(b) = a.checked_sub(r) {
                if b & 0xffff == 0 {
                    cands.insert(b);
                }
            }
        }
    }
    let (b, s) = cands.into_iter().map(|b| (b, score(b))).max_by_key(|&(b, s)| (s, b == pe.preferred_base))?;
    (2 * s >= rvas.len()).then_some(b)
}

/// The reachable slots of unit `u`, built from the PE image `bytes`.
pub fn reach(u: &gex_split::Unit, bytes: &[u8]) -> Result<Reach> {
    let pe = gex_ld::pe::parse(bytes).map_err(|e| anyhow::anyhow!("{e}"))?;
    let addrs = u.addresses();
    let slot_at: HashMap<u64, u32> = addrs.iter().enumerate().map(|(k, &a)| (a, k as u32)).collect();
    let addr_set: HashSet<u64> = addrs.iter().copied().collect();
    let Some(base) = find_base(&pe, &addr_set) else {
        return Ok(whole(u, pe.preferred_base, "the unit's base is not found from the image's exports"));
    };
    if pe.relocs.is_empty() {
        return Ok(whole(u, base, "no base relocations"));
    }
    if pe.dirs.get(14).is_some_and(|&(rva, size)| rva != 0 && size != 0) {
        return Ok(whole(u, base, "a CLR header"));
    }
    let (image, seeds) = gex_xlate::func::pe_input(&pe, base).map_err(|e| anyhow::anyhow!("placing the image at {base:#x}: {e}"))?;
    let prog = gex_xlate::func::recover_pe(&image, &seeds, &Default::default());

    // Where each address is: the functions whose entry, interior entry or
    // block holds it (shared code is in several).
    let mut at_entry: HashMap<u64, Vec<u64>> = HashMap::new();
    let mut spans: Vec<(u64, u64, u64)> = Vec::new();
    for (&e, f) in &prog.functions {
        at_entry.entry(e).or_default().push(e);
        for &x in &f.entries {
            at_entry.entry(x).or_default().push(e);
        }
        for b in &f.blocks {
            if b.end > b.start {
                spans.push((b.start, b.end, e));
            }
        }
    }
    spans.sort_unstable();
    // The longest block bounds how far back a holder can start.
    let longest = spans.iter().map(|&(s, e, _)| e - s).max().unwrap_or(0);
    let holders = |a: u64| -> Vec<u64> {
        let mut out: Vec<u64> = at_entry.get(&a).cloned().unwrap_or_default();
        let lo = spans.partition_point(|&(s, _, _)| s < a.saturating_sub(longest));
        for &(s, e, f) in &spans[lo..] {
            if s > a {
                break;
            }
            if a < e {
                out.push(f);
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    };

    // A get-PC idiom: a call whose target is its own continuation.
    for f in prog.functions.values() {
        for b in &f.blocks {
            if let FTerm::Call { callee: Callee::Direct(t), ret_pc, .. } = &b.term {
                if t == ret_pc {
                    return Ok(whole(u, base, format!("a get-PC call at {:#x}", b.term_pc)));
                }
            }
        }
    }

    if std::env::var_os("GEX_PRUNE_TRACE").is_some() {
        // Which sources the functions' own starts have: how many functions
        // have only a call, and how many each other source makes roots
        // (alone or with others).
        let mut only_call = 0usize;
        let mut by: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for &e in prog.functions.keys() {
            let Some(info) = prog.starts.get(&e) else {
                *by.entry("(no start)".into()).or_default() = (by.get("(no start)").map_or(0, |x| x.0) + 1, 0);
                continue;
            };
            let srcs: Vec<Source> = ROOT_SOURCES.iter().copied().filter(|&s| info.has(s)).collect();
            if srcs.is_empty() {
                only_call += 1;
            }
            for &s in &srcs {
                let x = by.entry(format!("{s:?}")).or_default();
                x.0 += 1;
                if srcs.len() == 1 {
                    x.1 += 1;
                }
            }
        }
        eprintln!("[prune-src] {:#x}: {} functions, {only_call} only called; by source (any, sole): {by:?}", base, prog.functions.len());
    }
    let mut reached: HashSet<u64> = HashSet::new();
    let mut q: VecDeque<u64> = VecDeque::new();
    let mut slot_q: VecDeque<u32> = VecDeque::new();
    let mut slot_done: HashSet<u32> = HashSet::new();
    let mut roots = 0usize;
    for (&a, info) in &prog.starts {
        if ROOT_SOURCES.iter().any(|&s| info.has(s)) {
            for f in holders(a) {
                if reached.insert(f) {
                    roots += 1;
                    q.push_back(f);
                }
            }
        }
    }
    // Slots recovery does not account for: kept, and followed through the
    // unit's own calls.
    let mut unknown = 0usize;
    for (k, &a) in addrs.iter().enumerate() {
        if holders(a).is_empty() {
            unknown += 1;
            slot_q.push_back(k as u32);
        }
    }
    let wasm_calls = u.slot_callees();
    let mut keep_unknown: HashSet<u32> = HashSet::new();
    loop {
        if let Some(e) = q.pop_front() {
            let f = &prog.functions[&e];
            let mut next: Vec<u64> = Vec::new();
            for b in &f.blocks {
                match &b.term {
                    FTerm::Call { callee, ret_pc, .. } => {
                        if let Callee::Direct(t) = callee {
                            next.push(*t);
                        }
                        next.push(*ret_pc);
                    }
                    FTerm::TailCall { callee: Callee::Direct(t) } => next.push(*t),
                    FTerm::Syscall { next_pc, .. } => next.push(*next_pc),
                    _ => {}
                }
            }
            // The slot's direct calls in the unit, and its interior entries'.
            for a in std::iter::once(e).chain(f.entries.iter().copied()) {
                if let Some(&k) = slot_at.get(&a) {
                    slot_q.push_back(k);
                }
            }
            for t in next {
                for g in holders(t) {
                    if reached.insert(g) {
                        q.push_back(g);
                    }
                }
            }
            continue;
        }
        if let Some(k) = slot_q.pop_front() {
            if !slot_done.insert(k) {
                continue;
            }
            let hs = holders(addrs[k as usize]);
            if hs.is_empty() {
                keep_unknown.insert(k);
            }
            for &c in &wasm_calls[k as usize] {
                for g in holders(addrs[c as usize]) {
                    if reached.insert(g) {
                        q.push_back(g);
                    }
                }
                if holders(addrs[c as usize]).is_empty() {
                    slot_q.push_back(c);
                }
            }
            continue;
        }
        break;
    }
    if std::env::var_os("GEX_PRUNE_TRACE").is_some() {
        // A diagnostic only (never a site's): the closure from the strong
        // roots alone (entry, exports, TLS, init arrays, import thunks), and
        // from those plus .eh_frame; what the address-taken roots cost.
        for (name, set) in [
            ("strong", &[Source::Entry, Source::Symbol, Source::InitArray, Source::ImportThunk][..]),
            ("strong+eh", &[Source::Entry, Source::Symbol, Source::InitArray, Source::ImportThunk, Source::EhFrame][..]),
        ] {
            let mut r: HashSet<u64> = HashSet::new();
            let mut q: VecDeque<u64> = VecDeque::new();
            for (&a, info) in &prog.starts {
                if set.iter().any(|&s| info.has(s)) {
                    for f in holders(a) {
                        if r.insert(f) {
                            q.push_back(f);
                        }
                    }
                }
            }
            while let Some(e) = q.pop_front() {
                for b in &prog.functions[&e].blocks {
                    let ts: Vec<u64> = match &b.term {
                        FTerm::Call { callee, ret_pc, .. } => match callee {
                            Callee::Direct(t) => vec![*t, *ret_pc],
                            _ => vec![*ret_pc],
                        },
                        FTerm::TailCall { callee: Callee::Direct(t) } => vec![*t],
                        FTerm::Syscall { next_pc, .. } => vec![*next_pc],
                        _ => vec![],
                    };
                    for t in ts {
                        for g in holders(t) {
                            if r.insert(g) {
                                q.push_back(g);
                            }
                        }
                    }
                }
            }
            eprintln!("[prune-diag] {base:#x}: closure from {name} roots: {} of {} functions", r.len(), prog.functions.len());
        }
        eprintln!("[prune-diag] {base:#x}: closure from all roots: {} of {} functions", reached.len(), prog.functions.len());
    }
    let mut keep = keep_unknown;
    for (k, &a) in addrs.iter().enumerate() {
        if holders(a).iter().any(|f| reached.contains(f)) {
            keep.insert(k as u32);
        }
    }
    Ok(Reach { keep, whole: None, base, functions: prog.functions.len(), roots, unknown })
}

/// A tar's files, by path: enough of ustar (and GNU long names, pax paths)
/// to read a bundle's images, following its symbolic and hard links.
pub struct Tar<'a> {
    data: &'a [u8],
    entries: BTreeMap<String, TarEntry>,
}

enum TarEntry {
    File(usize, usize),
    Dir,
    Sym(String),
    Hard(String),
}

fn field(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn octal(b: &[u8]) -> usize {
    let s = field(b);
    usize::from_str_radix(s.trim(), 8).unwrap_or(0)
}

fn clean(p: &str) -> String {
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

impl<'a> Tar<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Tar<'a>> {
        let mut entries = BTreeMap::new();
        let mut off = 0usize;
        let mut long: Option<String> = None;
        while off + 512 <= data.len() {
            let h = &data[off..off + 512];
            if h.iter().all(|&c| c == 0) {
                off += 512;
                continue;
            }
            let size = octal(&h[124..136]);
            let kind = h[156];
            let body = off + 512;
            let next = body + size.div_ceil(512) * 512;
            if next > data.len() {
                bail!("a tar entry runs past the end ({off:#x})");
            }
            let mut name = field(&h[0..100]);
            let prefix = field(&h[345..500]);
            if !prefix.is_empty() {
                name = format!("{prefix}/{name}");
            }
            if let Some(l) = long.take() {
                name = l;
            }
            let link = field(&h[157..257]);
            match kind {
                b'L' => long = Some(field(&data[body..body + size])),
                b'x' => {
                    // pax: a "path=" record replaces the next entry's name.
                    let text = String::from_utf8_lossy(&data[body..body + size]);
                    for rec in text.lines() {
                        if let Some((_, kv)) = rec.split_once(' ') {
                            if let Some(p) = kv.strip_prefix("path=") {
                                long = Some(p.to_string());
                            }
                        }
                    }
                }
                b'5' => {
                    entries.insert(clean(&name), TarEntry::Dir);
                }
                b'2' => {
                    entries.insert(clean(&name), TarEntry::Sym(link));
                }
                b'1' => {
                    entries.insert(clean(&name), TarEntry::Hard(clean(&link)));
                }
                b'0' | 0 | b'7' => {
                    entries.insert(clean(&name), TarEntry::File(body, size));
                }
                _ => {}
            }
            off = next;
        }
        Ok(Tar { data, entries })
    }

    /// A file's bytes by its guest path, following links.
    pub fn read(&self, path: &str) -> Option<&'a [u8]> {
        let mut parts: VecDeque<String> = clean(path).split('/').filter(|s| !s.is_empty()).map(String::from).collect();
        let mut at: Vec<String> = Vec::new();
        let mut hops = 0;
        while let Some(p) = parts.pop_front() {
            at.push(p);
            let cur = at.join("/");
            match self.entries.get(&cur) {
                Some(TarEntry::Sym(t)) => {
                    hops += 1;
                    if hops > 40 {
                        return None;
                    }
                    at.pop();
                    let target = if t.starts_with('/') { clean(t) } else { clean(&format!("{}/{t}", at.join("/"))) };
                    at = Vec::new();
                    for (i, s) in target.split('/').filter(|s| !s.is_empty()).enumerate() {
                        parts.insert(i, s.to_string());
                    }
                }
                Some(TarEntry::Hard(t)) if parts.is_empty() => return self.read(t),
                Some(TarEntry::File(o, n)) if parts.is_empty() => return Some(&self.data[*o..*o + *n]),
                Some(_) => {}
                None => return None,
            }
        }
        None
    }
}

/// Per-image numbers of a pruned build, for the report.
pub struct PruneStat {
    pub key: String,
    pub path: String,
    pub slots: usize,
    pub kept: usize,
    pub bytes_before: usize,
    pub bytes_after: usize,
    pub whole: Option<String>,
}

/// Prune one unit: its reachable slots from its image in the bundle.
pub fn prune_unit(wasm: &[u8], image: &[u8], key: &str, path: &str) -> Result<(Vec<u8>, PruneStat)> {
    let u = gex_split::Unit::parse(wasm).with_context(|| format!("unit {key}"))?;
    let r = reach(&u, image).with_context(|| format!("the reach of {path}"))?;
    let out = if r.keep.len() == u.slots.len() { wasm.to_vec() } else { gex_split::prune(wasm, &r.keep)? };
    let stat = PruneStat {
        key: key.to_string(),
        path: path.to_string(),
        slots: u.slots.len(),
        kept: r.keep.len(),
        bytes_before: wasm.len(),
        bytes_after: out.len(),
        whole: r.whole.clone(),
    };
    if std::env::var_os("GEX_PRUNE_TRACE").is_some() {
        eprintln!(
            "[prune] {path}: base {:#x}, {} functions, {} roots, {} unknown slots, {}/{} slots kept{}",
            r.base,
            r.functions,
            r.roots,
            r.unknown,
            r.keep.len(),
            u.slots.len(),
            r.whole.as_deref().map(|w| format!(" (whole: {w})")).unwrap_or_default()
        );
    }
    Ok((out, stat))
}
