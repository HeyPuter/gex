//! Cross-image fan-out reachability: which functions, across *all* the x86
//! images a site ships, no execution can reach (`"prune": "fanout"`).
//!
//! [`crate::reach`] proves one image at a time, so it has to assume anything
//! outside the image may call any export and may hold any relocated address.
//! In a PE32 every absolute address is relocated, so that assumption roots
//! almost every function and nothing is left out. This module drops it: the
//! site ships a *closed* set of images (the exe plus every DLL it can load),
//! so what reaches an image's code is exactly what the other images in the
//! set, and the runtime, do.
//!
//! # The proof
//!
//! Over all images together, to a fixed point:
//!
//! * **Roots.** The exe's entry; every image's entry and TLS callbacks (any
//!   image in the bundle may be loaded, so its initialisation runs).
//! * **An export** is reachable when a *reachable* import slot binds to it
//!   (an import table entry, a delay-import entry, or another reachable
//!   export forwarding to it); when a reachable address holds its name, which
//!   is what a `GetProcAddress` by name can find; or when its image is
//!   one nothing in the set imports, which the runtime therefore loaded by
//!   name from outside the set -- a service, a COM server, a proxy -- and
//!   entered at an export of. An ordinal-named export is reachable when a
//!   reachable address names its module, since nothing bounds which ordinal
//!   a lookup passes. `.eh_frame` and `.pdata` entries are never roots.
//! * **A relocation target** is reachable only when the relocation *site*
//!   lies in a reachable function or in a reachable data region. A site in a
//!   recovered function's extent is that function's, whether or not a decoded
//!   instruction covers it: recovery decodes *around* bytes it cannot read
//!   rather than over them, and in an image whose read-only data is merged
//!   into `.text` those gaps hold the vtables and the operands recovery could
//!   not place. The site's instruction, where there is one, says how far the
//!   reference reaches ([`Use`]): an absolute memory operand with no index
//!   touches only the pointer-sized word at the target, while an address that
//!   goes into a register or onto the stack may be used at any offset from
//!   the object it points into, so it reaches that whole object. A word-sized
//!   read of an address a function covers is a reference to the function:
//!   what sits there is an instruction, not a pointer.
//! The entry points the runtime calls itself need no rule of their own: a
//! callback registered with Wine, a window procedure reached through a
//! registered class and a COM vtable reached from a live object are all
//! *stored* addresses, so each is a relocation in code or in data that the
//! closure already follows from whatever reachable code registers it.
//!
//! # Objects
//!
//! An object's extent is not in the file, so it is bounded by the addresses
//! something stores a pointer to: a **region** runs from one relocation
//! target to the next (also cut at section bounds and at every byte a
//! recovered function covers). A reference the program may offset from
//! reaches the whole region holding the target; since an absolute reference
//! is itself relocated in a PE32, its target always starts a region.
//!
//! This is sound for a compiler that lays objects out contiguously and
//! reaches an object's interior from a pointer to its start. It is *not*
//! sound for an object whose interior some other absolute reference splits
//! and whose later half is only ever reached by arithmetic from its start; a
//! pruned function that runs reports itself (`gex.pruned`), so that shows up
//! as a trap naming the address rather than as silent corruption.
//!
//! # Computed code addresses
//!
//! An indirect branch recovery could not resolve needs no special rule. A
//! code address the program branches to is either stored somewhere -- and
//! every relocation in reachable code and reachable data is followed -- or
//! computed from one, which for x86 is a jump table read through an absolute
//! memory operand (the table is a region, and its entries are followed) or
//! the get-PC idiom (`call $+5`), which keeps the image whole.
//!
//! # Kept whole
//!
//! An image without base relocations (an absolute address could hide
//! anywhere), with a CLR header (managed code calls by token), with a get-PC
//! idiom, or whose unit and image cannot be lined up, is kept entirely, and
//! the report says which and why.

use anyhow::{Context, Result};
use gex_ir::func::{Callee, FTerm};
use gex_isa_x86::{Mem, Op, Operand};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

/// How far a reference reaches from its target.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Use {
    /// The address goes into a register or onto the stack: the program may
    /// use any offset from the object the target points into.
    Object,
    /// An absolute memory operand with no index register: only the
    /// pointer-sized word at the target is touched.
    Word,
}

/// What a relocation, or a recovered edge, reaches.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
enum Ref {
    /// A byte a recovered function covers, by the function's index.
    Code(u32),
    /// A data region, by index.
    Region(u32),
    /// The pointer-sized word at this address: whatever relocation sits
    /// there, and the import slot it may be.
    Word(u64),
}

/// An export of an image.
struct Export {
    name: Option<String>,
    ordinal: u32,
    /// Absolute address of the exported code, when it is in this image.
    addr: Option<u64>,
    /// `"OTHER.Name"` or `"OTHER.#13"`, when the export forwards.
    forwarder: Option<String>,
}

/// What an image imports at one slot.
struct Import {
    module: String,
    name: Option<String>,
    ordinal: Option<u32>,
}

/// One image's summary: everything the global closure needs, without its
/// bytes or its recovered program.
pub struct Image {
    /// The guest path the unit was built from.
    pub path: String,
    /// Lower-case file name, the key imports name it by.
    pub module: String,
    pub base: u64,
    /// Why the whole image is kept, when it is.
    pub whole: Option<String>,
    /// The image is an executable, not a DLL.
    is_exe: bool,
    entry: Option<u64>,
    tls: Vec<u64>,

    /// Recovered functions, by index: the entry address of each.
    entries: Vec<u64>,
    /// `(start, end, function)` of every block, sorted: which functions cover
    /// an address.
    spans: Vec<(u64, u64, u32)>,
    /// The longest block, which bounds how far back a covering block starts.
    longest: u64,
    /// Addresses that are a function's entry or interior entry.
    at_entry: HashMap<u64, Vec<u32>>,
    /// What each function reaches: its own calls and its relocations.
    func_refs: Vec<Vec<(Ref, Use)>>,
    /// Functions whose body holds an indirect branch recovery left
    /// unresolved, for the report.
    unresolved: usize,

    /// Data regions, `(start, end)`, sorted and disjoint.
    regions: Vec<(u64, u64)>,
    /// What the relocations inside each region reach.
    region_refs: Vec<Vec<(Ref, Use)>>,
    /// The names each region holds: what a lookup could pass.
    region_name: Vec<Vec<String>>,

    /// What the relocation at an address reaches, for a word-sized read.
    /// Several, where shared code puts the target in more than one function.
    word_ref: HashMap<u64, Vec<(Ref, Use)>>,
    /// Import slots, by the address of the slot.
    iat: HashMap<u64, u32>,
    imports: Vec<Import>,
    exports: Vec<Export>,
    /// Exports by the name and by the ordinal they answer to.
    by_name: HashMap<String, u32>,
    by_ordinal: HashMap<u32, u32>,
    /// The image has exports that only an ordinal names.
    ordinal_only: usize,

    /// `(start, end, function)` of each function's extent -- its first block
    /// to its last -- sorted: a relocation inside it that no block covers is
    /// still that function's, since recovery decoded around the bytes rather
    /// than over them.
    hulls: Vec<(u64, u64, u32)>,

    /// Every absolute address the image holds, as `(site, target)`, sorted:
    /// kept for the diagnosis of a function the closure did not reach.
    relocs: Vec<(u64, u64)>,

    /// The unit's slots: their addresses, and its own calls between them.
    slot_addr: Vec<u64>,
    slot_of: HashMap<u64, u32>,
    wasm_calls: Vec<Vec<u32>>,
}

impl Image {
    /// An index for the diagnosis: which relocation sites point at each
    /// address, and which functions refer to each function.
    pub fn back_index(&self) -> (HashMap<u64, Vec<u64>>, HashMap<u32, Vec<u32>>) {
        let mut by_target: HashMap<u64, Vec<u64>> = HashMap::new();
        for &(site, target) in &self.relocs {
            by_target.entry(target).or_default().push(site);
        }
        let mut callers: HashMap<u32, Vec<u32>> = HashMap::new();
        for (g, refs) in self.func_refs.iter().enumerate() {
            for &(r, _) in refs {
                if let Ref::Code(f) = r {
                    callers.entry(f).or_default().push(g as u32);
                }
            }
        }
        (by_target, callers)
    }

    /// Whether anything the closure *did* reach refers to the function
    /// covering `addr`. A function the closure missed whose every referrer
    /// it also missed is a consequence; one a reached function or region
    /// points at, or a reached function calls, is the rule that is wrong.
    pub fn frontier(&self, addr: u64, ke: &Kept, ix: &(HashMap<u64, Vec<u64>>, HashMap<u32, Vec<u32>>)) -> bool {
        for &site in ix.0.get(&addr).map_or(&[][..], |v| v) {
            match self.region_at(site) {
                Some(r) => {
                    if ke.reached_regions.contains(&r) {
                        return true;
                    }
                }
                None => {
                    if self.holders(site).iter().any(|f| ke.reached_funcs.contains(f)) {
                        return true;
                    }
                }
            }
        }
        if let Some(&Ref::Code(f)) = self.at(addr).first() {
            if ix.1.get(&f).is_some_and(|gs| gs.iter().any(|g| ke.reached_funcs.contains(g))) {
                return true;
            }
        }
        false
    }

    /// Why the closure did not reach the function covering `addr`: what
    /// refers to it, and whether each referrer was itself reached.
    pub fn why(&self, addr: u64, ke: &Kept, ix: &(HashMap<u64, Vec<u64>>, HashMap<u32, Vec<u32>>)) -> String {
        let mut out: Vec<String> = Vec::new();
        if let Some(e) = self.exports.iter().find(|e| e.addr == Some(addr)) {
            out.push(format!(
                "an export ({}) nothing looked up",
                e.name.clone().unwrap_or_else(|| format!("#{}", e.ordinal))
            ));
        }
        // Every absolute address in the image that points at it, and where
        // that pointer sits.
        let (mut sites, mut in_reached_func, mut in_pruned_func) = (0, 0, 0);
        let (mut in_reached_region, mut in_pruned_region, mut nowhere) = (0, 0, 0);
        for &site in ix.0.get(&addr).map_or(&[][..], |v| v) {
            sites += 1;
            let hs = if self.region_at(site).is_some() { Vec::new() } else { self.holders(site) };
            if !hs.is_empty() {
                if hs.iter().any(|f| ke.reached_funcs.contains(f)) {
                    in_reached_func += 1;
                } else {
                    in_pruned_func += 1;
                }
            } else if let Some(r) = self.region_at(site) {
                if ke.reached_regions.contains(&r) {
                    in_reached_region += 1;
                } else {
                    in_pruned_region += 1;
                }
            } else {
                nowhere += 1;
            }
        }
        // What refers to the function at all, reached or not.
        let callers: Vec<u32> = match self.at(addr).first() {
            Some(&Ref::Code(f)) => ix.1.get(&f).cloned().unwrap_or_default(),
            _ => Vec::new(),
        };
        let live = callers.iter().filter(|g| ke.reached_funcs.contains(g)).count();
        if self.at(addr).is_empty() {
            out.push("no recovered function covers it".into());
        }
        out.push(format!(
            "{sites} pointer(s) to it ({in_reached_func} in a reached function, {in_pruned_func} in a pruned one, \
             {in_reached_region} in a reached region, {in_pruned_region} in a pruned region, {nowhere} nowhere); \
             {} referrer(s), {live} reached",
            callers.len()
        ));
        out.join("; ")
    }

    /// The unit's slot addresses, by slot index.
    pub fn slot_addrs(&self) -> &[u64] {
        &self.slot_addr
    }

    /// The functions covering an address: its holders at an entry, and every
    /// block whose range holds it.
    fn holders(&self, a: u64) -> Vec<u32> {
        let mut out: Vec<u32> = self.at_entry.get(&a).cloned().unwrap_or_default();
        let lo = self.spans.partition_point(|&(s, _, _)| s < a.saturating_sub(self.longest));
        for &(s, e, f) in &self.spans[lo..] {
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
    }

    /// The functions whose extent holds an address, when no block does.
    fn hull_holders(&self, a: u64) -> Vec<u32> {
        let mut out = Vec::new();
        for &(s, e, f) in &self.hulls {
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
    }

    /// The region holding an address.
    fn region_at(&self, a: u64) -> Option<u32> {
        let k = self.regions.partition_point(|&(s, _)| s <= a);
        let k = k.checked_sub(1)?;
        (a < self.regions[k].1).then_some(k as u32)
    }

    /// What an address reaches: the functions covering it, or the region
    /// holding it.
    ///
    /// Inside a pointer table it is both. A table's relocations are the
    /// region's, since no instruction covers them, so a pointer to the table
    /// has to reach the region or its entries are lost; but a run of
    /// relocated words can also fall over real code, and resolving to the
    /// region alone would lose the function and everything it calls. Taking
    /// both costs only what the wrong one of the two drags in.
    fn at(&self, a: u64) -> Vec<Ref> {
        let mut out: Vec<Ref> = self.holders(a).into_iter().map(Ref::Code).collect();
        if out.is_empty() {
            out = self.hull_holders(a).into_iter().map(Ref::Code).collect();
        }
        if out.is_empty() {
            out.extend(self.region_at(a).map(Ref::Region));
        }
        out
    }
}

/// The NUL-terminated names at `off` in the mapped image: the bytes read as
/// ASCII, and read as UTF-16 whose every character is ASCII, since both are
/// plausible and only the program knows which it is. A wide string's first
/// byte is an ASCII character followed by a NUL, so reading only the first
/// plausible encoding finds `"U"` where the program holds `L"UXInit.dll"` --
/// and the module that names goes unkept.
fn names_at(img: &[u8], off: usize) -> Vec<String> {
    let mut out = Vec::new();
    let Some(rest) = img.get(off..) else { return out };
    if let Some(n) = rest.iter().position(|&c| c == 0).filter(|&n| n > 0 && n <= 255) {
        let b = &rest[..n];
        if b.iter().all(|&c| (0x21..0x7f).contains(&c)) {
            out.push(String::from_utf8_lossy(b).into_owned());
        }
    }
    let mut w = String::new();
    let mut i = 0;
    while i + 1 < rest.len() && w.len() <= 255 {
        let c = u16::from_le_bytes([rest[i], rest[i + 1]]);
        if c == 0 {
            break;
        }
        if !(0x21..0x7f).contains(&c) {
            w.clear();
            break;
        }
        w.push(c as u8 as char);
        i += 2;
    }
    if !w.is_empty() && i + 1 < rest.len() {
        out.push(w);
    }
    out.dedup();
    out
}

/// Every name a region holds: what a lookup inside it could pass. A region
/// is usually one object, but an array of names is one too, so the whole
/// region is read, not only its start.
fn region_names(img: &[u8], base: u64, start: u64, end: u64) -> Vec<String> {
    const MAX_REGION: u64 = 4096;
    const MAX_NAMES: usize = 32;
    let mut out: Vec<String> = Vec::new();
    let Some(lo) = start.checked_sub(base) else { return out };
    let hi = (end - start).min(MAX_REGION) + lo;
    let mut off = lo as usize;
    while off < hi as usize && out.len() < MAX_NAMES {
        for n in names_at(img, off) {
            if !out.contains(&n) {
                out.push(n);
            }
        }
        // On to the byte after this string, or the next one.
        let step = img[off..(hi as usize).min(img.len())]
            .iter()
            .position(|&c| c == 0)
            .map_or(usize::MAX, |n| n + 1);
        if step == usize::MAX {
            break;
        }
        off += step.max(1);
    }
    out
}

/// Classify the relocation at `site`, whose target is `target`, by the
/// instruction holding it: which operand carries the address, and so how far
/// the reference reaches. A relocation no instruction covers is a stored
/// pointer, which the program may offset from.
fn classify(img: &[u8], base: u64, inst_pc: Option<u64>, target: u64) -> Use {
    let Some(pc) = inst_pc else { return Use::Object };
    let Some(off) = pc.checked_sub(base).map(|o| o as usize) else { return Use::Object };
    let Some(bytes) = img.get(off..(off + 16).min(img.len())) else { return Use::Object };
    let inst = gex_isa_x86::decode::decode_in(gex_isa_x86::Mode::Compat, bytes, pc);
    if !inst.is_known() {
        return Use::Object;
    }
    // `lea` computes the address rather than reading it.
    let reads = !matches!(inst.op, Op::Lea);
    for o in inst.ops.iter().flatten().copied() {
        match o {
            Operand::Mem(Mem { base: None, index: None, disp, .. }) if disp as u64 as u32 == target as u32 => {
                if reads {
                    return Use::Word;
                }
                return Use::Object;
            }
            _ => {}
        }
    }
    Use::Object
}

/// Summarise one image: its functions and what each reaches, its data
/// regions, its imports and exports.
pub fn summarize(unit: &gex_split::Unit, bytes: &[u8], path: &str) -> Result<Image> {
    let module = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let pe = gex_ld::pe::parse(bytes).map_err(|e| anyhow::anyhow!("{e}"))?;
    let slot_addr = unit.addresses();
    let addr_set: HashSet<u64> = slot_addr.iter().copied().collect();
    let wasm_calls = unit.slot_callees();
    let nothing = |base: u64, why: &str| Image {
        path: path.to_string(),
        module: module.clone(),
        base,
        whole: Some(why.to_string()),
        is_exe: false,
        entry: None,
        tls: Vec::new(),
        entries: Vec::new(),
        spans: Vec::new(),
        longest: 0,
        at_entry: HashMap::new(),
        func_refs: Vec::new(),
        unresolved: 0,
        regions: Vec::new(),
        region_refs: Vec::new(),
        region_name: Vec::new(),
        word_ref: HashMap::new(),
        iat: HashMap::new(),
        imports: Vec::new(),
        exports: Vec::new(),
        by_name: HashMap::new(),
        by_ordinal: HashMap::new(),
        ordinal_only: 0,
        slot_addr: slot_addr.clone(),
        slot_of: slot_addr.iter().enumerate().map(|(k, &a)| (a, k as u32)).collect(),
        hulls: Vec::new(),
        relocs: Vec::new(),
        wasm_calls: wasm_calls.clone(),
    };
    let Some(base) = crate::reach::find_base(&pe, &addr_set) else {
        return Ok(nothing(pe.preferred_base, "the unit's base is not found from the image's exports"));
    };
    if pe.relocs.is_empty() {
        return Ok(nothing(base, "no base relocations"));
    }
    if pe.dirs.get(14).is_some_and(|&(rva, size)| rva != 0 && size != 0) {
        return Ok(nothing(base, "a CLR header"));
    }
    let img = pe.map(base).map_err(|e| anyhow::anyhow!("placing the image at {base:#x}: {e}"))?;
    let (image, seeds) = gex_xlate::func::pe_input(&pe, base).map_err(|e| anyhow::anyhow!("placing the image at {base:#x}: {e}"))?;
    let prog = gex_xlate::func::recover_pe(&image, &seeds, &Default::default());

    // ---- functions -------------------------------------------------------
    let mut entries: Vec<u64> = prog.functions.keys().copied().collect();
    entries.sort_unstable();
    let fidx: HashMap<u64, u32> = entries.iter().enumerate().map(|(k, &a)| (a, k as u32)).collect();
    let mut at_entry: HashMap<u64, Vec<u32>> = HashMap::new();
    let mut spans: Vec<(u64, u64, u32)> = Vec::new();
    for (&e, f) in &prog.functions {
        let k = fidx[&e];
        at_entry.entry(e).or_default().push(k);
        for &x in &f.entries {
            at_entry.entry(x).or_default().push(k);
        }
        for b in &f.blocks {
            if b.end > b.start {
                spans.push((b.start, b.end, k));
            }
        }
    }
    spans.sort_unstable();
    let longest = spans.iter().map(|&(s, e, _)| e - s).max().unwrap_or(0);
    let mut hulls: Vec<(u64, u64, u32)> = Vec::new();
    for (&e, f) in &prog.functions {
        let k = fidx[&e];
        let lo = f.blocks.iter().map(|b| b.start).chain(std::iter::once(e)).min().unwrap_or(e);
        let hi = f.blocks.iter().map(|b| b.end).max().unwrap_or(e);
        if hi > lo {
            hulls.push((lo, hi, k));
        }
    }
    hulls.sort_unstable();

    // A get-PC idiom computes code addresses from a return address: nothing
    // bounds what the image can reach.
    for f in prog.functions.values() {
        for b in &f.blocks {
            if let FTerm::Call { callee: Callee::Direct(t), ret_pc, .. } = &b.term {
                if t == ret_pc {
                    return Ok(nothing(base, &format!("a get-PC call at {:#x}", b.term_pc)));
                }
            }
        }
    }

    let mut me = nothing(base, "");
    me.whole = None;
    me.base = base;
    me.is_exe = pe.characteristics & 0x2000 == 0;
    me.entry = (pe.entry_rva != 0).then(|| base + pe.entry_rva as u64);
    me.entries = entries;
    me.at_entry = at_entry;
    me.longest = longest;
    me.hulls = hulls;

    // ---- imports and exports --------------------------------------------
    for d in &pe.imports {
        for s in &d.symbols {
            let (name, ordinal) = match &s.import {
                gex_ld::pe::ImportName::Name { name, .. } => (Some(name.clone()), None),
                gex_ld::pe::ImportName::Ordinal(o) => (None, Some(*o as u32)),
            };
            me.iat.insert(base + s.iat_rva as u64, me.imports.len() as u32);
            me.imports.push(Import { module: d.dll.to_ascii_lowercase(), name, ordinal });
        }
    }
    for d in &pe.delay_imports {
        for s in &d.symbols {
            let (name, ordinal) = match &s.import {
                gex_ld::pe::ImportName::Name { name, .. } => (Some(name.clone()), None),
                gex_ld::pe::ImportName::Ordinal(o) => (None, Some(*o as u32)),
            };
            me.iat.insert(base + s.iat_rva as u64, me.imports.len() as u32);
            me.imports.push(Import { module: d.dll.to_ascii_lowercase(), name, ordinal });
        }
    }
    for e in &pe.exports.entries {
        let k = me.exports.len() as u32;
        let name = e.names.first().cloned();
        if let Some(n) = &name {
            me.by_name.insert(n.clone(), k);
        } else {
            me.ordinal_only += 1;
        }
        me.by_ordinal.insert(e.ordinal, k);
        me.exports.push(Export {
            name,
            ordinal: e.ordinal,
            addr: (e.rva != 0 && e.forwarder.is_none()).then(|| base + e.rva as u64),
            forwarder: e.forwarder.clone(),
        });
    }
    if let Some(t) = &pe.tls {
        // The callback array is a NUL-terminated list of absolute addresses.
        if t.callbacks_va != 0 {
            let mut o = t.callbacks_va.wrapping_sub(pe.preferred_base) as usize;
            while let Some(b) = img.get(o..o + 4) {
                let v = u32::from_le_bytes(b.try_into().unwrap()) as u64;
                if v == 0 {
                    break;
                }
                me.tls.push(v);
                o += 4;
                if me.tls.len() > 64 {
                    break;
                }
            }
        }
    }

    // ---- relocations -----------------------------------------------------
    // Every absolute address the image holds, as (site, target).
    let ptr = 4usize;
    let mut relocs: Vec<(u64, u64)> = Vec::with_capacity(pe.relocs.len());
    for r in &pe.relocs {
        let off = r.rva as usize;
        let Some(b) = img.get(off..off + ptr) else { continue };
        let target = u32::from_le_bytes(b.try_into().unwrap()) as u64;
        relocs.push((base + r.rva as u64, target));
    }
    relocs.sort_unstable();

    // Where the image's code is. A recovered instruction outside an
    // executable section is a misdecode of data, and letting it count as
    // code cuts the object it sits in: ole32's and actxprxy's `.rdata` hold
    // arrays of COM stub vtables, and the bogus instructions in them split
    // each array into a region per entry, so a pointer to the array reached
    // only its first entry and every proxy behind it was lost.
    let exec: Vec<(u64, u64)> = pe
        .sections
        .iter()
        .filter(|s| s.executable() && s.vsize.max(s.raw_size) > 0)
        .map(|s| {
            let a = base + s.rva as u64;
            (a, a + s.vsize.max(s.raw_size) as u64)
        })
        .collect();
    let in_exec = |a: u64| exec.iter().any(|&(s, e)| a >= s && a < e);

    // ---- regions ---------------------------------------------------------
    // What a decoded instruction covers is code; everything else, including
    // the jump tables, vtables and literals an image with no `.rdata` keeps
    // between its functions and inside their block ranges, is data. (Paint's
    // images merge their read-only data into `.text`: of mspaint.exe's
    // 17,792 relocations, 17,396 are in an executable section.) The data is
    // cut into regions at the addresses something stores a pointer to, and
    // at section bounds.
    let mut covered: Vec<(u64, u64)> = Vec::new();
    for f in prog.functions.values() {
        for b in &f.blocks {
            for (i, &pc) in b.insts.iter().enumerate() {
                if !in_exec(pc) {
                    continue;
                }
                covered.push((pc, b.insts.get(i + 1).copied().unwrap_or(b.end).max(pc)));
            }
        }
    }
    covered.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (s, e) in covered {
        match merged.last_mut() {
            Some(l) if s <= l.1 => l.1 = l.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    me.spans = spans;
    let mut bounds: BTreeSet<u64> = BTreeSet::new();
    for s in &pe.sections {
        let n = s.vsize.max(s.raw_size) as u64;
        if n > 0 {
            bounds.insert(base + s.rva as u64);
            bounds.insert(base + s.rva as u64 + n);
        }
    }
    for &(_, t) in &relocs {
        bounds.insert(t);
    }
    for &(s, e) in &merged {
        bounds.insert(s);
        bounds.insert(e);
    }
    let image_end = base + pe.image_size as u64;
    let in_image = |a: u64| a >= base && a < image_end;
    let bs: Vec<u64> = bounds.iter().copied().filter(|&a| in_image(a)).collect();
    for w in bs.windows(2) {
        let (s, e) = (w[0], w[1]);
        if e <= s {
            continue;
        }
        // Not inside a function: a region of data.
        let k = merged.partition_point(|&(ms, _)| ms <= s);
        let inside = k.checked_sub(1).is_some_and(|k| s < merged[k].1);
        if !inside {
            me.regions.push((s, e));
        }
    }
    me.regions.sort_unstable();
    me.region_refs = vec![Vec::new(); me.regions.len()];
    me.region_name = me
        .regions
        .iter()
        .map(|&(s, e)| region_names(&img, base, s, e))
        .collect();

    let all_object = std::env::var_os("GEX_FANOUT_ALL_OBJECT").is_some();

    // ---- what each relocation reaches ------------------------------------
    // Every recovered instruction's address, to find the one covering a
    // relocation site.
    let mut insts: Vec<u64> = Vec::new();
    for f in prog.functions.values() {
        for b in &f.blocks {
            insts.extend(b.insts.iter().copied().filter(|&pc| in_exec(pc)));
        }
    }
    insts.sort_unstable();
    insts.dedup();
    let inst_at = |site: u64| -> Option<u64> {
        let k = insts.partition_point(|&a| a <= site).checked_sub(1)?;
        let pc = insts[k];
        (site - pc < 15).then_some(pc)
    };
    let mut func_refs: Vec<Vec<(Ref, Use)>> = vec![Vec::new(); me.entries.len()];
    for &(site, target) in &relocs {
        if !in_image(target) {
            continue;
        }
        let inst = inst_at(site);
        // GEX_FANOUT_ALL_OBJECT=1 drops the instruction's bound on every
        // reference: a diagnostic that separates a wrong classification from
        // a wrong graph. What it still misses is structural.
        let u = if all_object { Use::Object } else { classify(&img, base, inst, target) };
        let at = me.at(target);
        // `Use::Word` says only the pointer-sized word at the target is
        // touched, which bounds what a *data* reference reaches. An address
        // a recovered function covers is code: the word at it is an
        // instruction, not a pointer, so the reference is to the function,
        // however the instruction spells it. Read as a word, the function
        // was never reached at all and everything it calls went with it.
        let code = matches!(at.first(), Some(Ref::Code(_)));
        let what: Vec<(Ref, Use)> = match u {
            Use::Word if !code => vec![(Ref::Word(target), Use::Word)],
            _ => at.into_iter().map(|r| (r, Use::Object)).collect(),
        };
        if what.is_empty() {
            continue;
        }
        let mut hs = me.holders(site);
        if hs.is_empty() {
            hs = me.hull_holders(site);
        }
        if !hs.is_empty() {
            for f in hs {
                func_refs[f as usize].extend(what.iter().copied());
            }
        } else if let Some(r) = me.region_at(site) {
            me.region_refs[r as usize].extend(what.iter().copied());
        }
    }
    // The relocation at an address, for a word-sized read of it: a stored
    // pointer the program may offset from.
    for &(site, target) in &relocs {
        if !in_image(target) {
            continue;
        }
        let rs: Vec<(Ref, Use)> = me.at(target).into_iter().map(|r| (r, Use::Object)).collect();
        if !rs.is_empty() {
            me.word_ref.insert(site, rs);
        }
    }

    // ---- recovered edges -------------------------------------------------
    for (&e, f) in &prog.functions {
        let k = fidx[&e] as usize;
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
                FTerm::Jump { .. } | FTerm::Branch { .. } | FTerm::Switch { .. } => {}
                _ => {}
            }
        }
        for t in next {
            for r in me.at(t) {
                func_refs[k].push((r, Use::Object));
            }
        }
    }
    me.unresolved = prog.stats.unresolved;
    for v in &mut func_refs {
        v.sort_unstable_by_key(|&(r, u)| {
            (
                match r {
                    Ref::Code(k) => (0u8, k as u64),
                    Ref::Region(k) => (1, k as u64),
                    Ref::Word(a) => (2, a),
                },
                u as u8,
            )
        });
        v.dedup();
    }
    for v in &mut me.region_refs {
        v.sort_unstable_by_key(|&(r, u)| {
            (
                match r {
                    Ref::Code(k) => (0u8, k as u64),
                    Ref::Region(k) => (1, k as u64),
                    Ref::Word(a) => (2, a),
                },
                u as u8,
            )
        });
        v.dedup();
    }
    me.func_refs = func_refs;
    me.relocs = relocs;
    me.slot_of = slot_addr.iter().enumerate().map(|(k, &a)| (a, k as u32)).collect();
    me.slot_addr = slot_addr;
    me.wasm_calls = wasm_calls;
    Ok(me)
}

/// Every NUL-terminated name a file holds, ASCII or ASCII-in-UTF-16.
///
/// The site's own native code -- the Wine core and the loaders built as
/// wasm32 -- is not a unit and is never pruned, but it reaches a PE image's
/// exports, and so it can look one up by name. Nothing bounds which of its
/// strings reach `GetProcAddress`, so every name it holds counts as a
/// lookup that may succeed.
pub fn names_in(bytes: &[u8]) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut i = 0usize;
    while i < bytes.len() {
        // ASCII.
        let mut j = i;
        while j < bytes.len() && (0x21..0x7f).contains(&bytes[j]) {
            j += 1;
        }
        if j > i && j - i <= 255 && bytes.get(j) == Some(&0) {
            out.insert(String::from_utf8_lossy(&bytes[i..j]).into_owned());
        }
        // ASCII in UTF-16.
        let mut k = i;
        let mut w = String::new();
        while k + 1 < bytes.len() && bytes[k + 1] == 0 && (0x21..0x7f).contains(&bytes[k]) {
            w.push(bytes[k] as char);
            k += 2;
        }
        if !w.is_empty() && w.len() <= 255 && bytes.get(k..k + 2) == Some(&[0, 0]) {
            out.insert(w);
        }
        i = j.max(i + 1);
    }
    out
}

/// Why something conservative was kept, counted for the report.
#[derive(Default)]
pub struct Keeps {
    /// Images kept whole, by reason.
    pub whole: Vec<(String, String)>,
    /// Exports kept because a reachable address holds their name: a
    /// `GetProcAddress` could find them.
    pub name_lookup: usize,
    /// Images all of whose ordinal-named exports were kept, because
    /// reachable code or data names the module and an ordinal lookup
    /// cannot be bounded.
    pub ordinal_lookup: Vec<(String, usize)>,
    /// Slots recovery does not account for, kept with everything they reach.
    pub unknown_slots: usize,
    /// Reachable functions holding an indirect branch recovery left
    /// unresolved (sound: see the module docs, but worth watching).
    pub unresolved: usize,
    /// Data regions marked reachable, and their bytes.
    pub regions: usize,
    pub region_bytes: u64,
    /// Exports a reachable import slot binds to.
    pub import_bound: usize,
    /// Exports kept because the site's own native code holds their name.
    pub native_lookup: usize,
    /// Images nothing in the set imports: every export is a root.
    pub unimported: Vec<(String, usize)>,
}

/// What the closure kept, per image.
pub struct Kept {
    pub path: String,
    pub module: String,
    /// Slots to keep, by slot index in the unit.
    pub keep: HashSet<u32>,
    pub slots: usize,
    pub functions: usize,
    pub reached: usize,
    pub whole: Option<String>,
    pub exports: usize,
    pub exports_kept: usize,
    /// Slots kept.
    pub kept: usize,
    /// The functions and data regions the closure reached.
    pub reached_funcs: HashSet<u32>,
    pub reached_regions: HashSet<u32>,
}

/// A node of the global worklist.
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
enum Node {
    Func(u32, u32),
    Region(u32, u32),
    Word(u32, u64),
    Export(u32, u32),
    Slot(u32, u32),
}

/// What a lookup by `name` can find: the exports of that name anywhere in the
/// set, and, when the name is a module's, that image's ordinal-named exports
/// -- nothing bounds which ordinal a lookup passes.
#[allow(clippy::too_many_arguments)]
fn lookup(
    name: &str,
    imgs: &[Image],
    by_name: &HashMap<&str, Vec<(u32, u32)>>,
    by_module: &HashMap<&str, u32>,
    q: &mut VecDeque<Node>,
    seen: &mut HashSet<Node>,
    named_modules: &mut HashSet<u32>,
    ordinal_pending: &mut Vec<u32>,
    keeps: &mut Keeps,
    counter: fn(&mut Keeps) -> &mut usize,
) {
    if let Some(ts) = by_name.get(name) {
        for &(j, e) in ts {
            if seen.insert(Node::Export(j, e)) {
                q.push_back(Node::Export(j, e));
                *counter(keeps) += 1;
            }
        }
    }
    let low = name.to_ascii_lowercase();
    for cand in [low.clone(), format!("{low}.dll"), format!("{low}.exe")] {
        if let Some(&j) = by_module.get(cand.as_str()) {
            if imgs[j as usize].ordinal_only > 0 && named_modules.insert(j) {
                ordinal_pending.push(j);
                keeps.ordinal_lookup.push((imgs[j as usize].path.clone(), imgs[j as usize].ordinal_only));
            }
        }
    }
}

/// Run the closure over all the images together.
pub fn closure(imgs: &[Image], native: &HashSet<String>) -> (Vec<Kept>, Keeps) {
    let mut keeps = Keeps::default();
    // Exports by name, across every image: what a lookup by name can find.
    let mut by_name: HashMap<&str, Vec<(u32, u32)>> = HashMap::new();
    let mut by_module: HashMap<&str, u32> = HashMap::new();
    for (i, im) in imgs.iter().enumerate() {
        by_module.insert(im.module.as_str(), i as u32);
        for (k, e) in im.exports.iter().enumerate() {
            if let Some(n) = &e.name {
                by_name.entry(n.as_str()).or_default().push((i as u32, k as u32));
            }
        }
    }
    // A module name a reachable address holds: an image something may load
    // and look an ordinal up in.
    let mut named_modules: HashSet<u32> = HashSet::new();

    let mut seen: HashSet<Node> = HashSet::new();
    let mut q: VecDeque<Node> = VecDeque::new();
    let push = |q: &mut VecDeque<Node>, seen: &mut HashSet<Node>, n: Node| {
        if seen.insert(n) {
            q.push_back(n);
        }
    };

    // ---- roots -----------------------------------------------------------
    for (i, im) in imgs.iter().enumerate() {
        if im.whole.is_some() {
            keeps.whole.push((im.path.clone(), im.whole.clone().unwrap()));
            continue;
        }
        for a in im.entry.iter().chain(im.tls.iter()) {
            for r in im.at(*a) {
                match r {
                    Ref::Code(f) => push(&mut q, &mut seen, Node::Func(i as u32, f)),
                    Ref::Region(r) => push(&mut q, &mut seen, Node::Region(i as u32, r)),
                    Ref::Word(a) => push(&mut q, &mut seen, Node::Word(i as u32, a)),
                }
            }
        }
        // A slot recovery does not account for is kept, and followed through
        // the unit's own calls.
        for (k, &a) in im.slot_addr.iter().enumerate() {
            if im.holders(a).is_empty() {
                keeps.unknown_slots += 1;
                push(&mut q, &mut seen, Node::Slot(i as u32, k as u32));
            }
        }
    }

    // An image nothing in the set imports is one the runtime loaded by name
    // from outside it -- a service, a COM server, a proxy -- and called an
    // export of. Which export is not in any image, so they are all roots.
    {
        let mut imported: HashSet<&str> = HashSet::new();
        for im in imgs {
            for i in &im.imports {
                imported.insert(i.module.as_str());
            }
        }
        for (j, im) in imgs.iter().enumerate() {
            if im.is_exe || imported.contains(im.module.as_str()) || im.exports.is_empty() {
                continue;
            }
            keeps.unimported.push((im.path.clone(), im.exports.len()));
            for e in 0..im.exports.len() as u32 {
                if seen.insert(Node::Export(j as u32, e)) {
                    q.push_back(Node::Export(j as u32, e));
                }
            }
        }
    }

    // The site's own native code holds names it may look up.
    let mut ordinal_pending: Vec<u32> = Vec::new();
    for nm in native {
        lookup(
            nm,
            imgs,
            &by_name,
            &by_module,
            &mut q,
            &mut seen,
            &mut named_modules,
            &mut ordinal_pending,
            &mut keeps,
            |k| &mut k.native_lookup,
        );
    }

    // ---- the fixed point -------------------------------------------------
    loop {
        let Some(n) = q.pop_front() else {
            // An image whose module name a reachable address holds may be
            // loaded by name and looked up by ordinal, which nothing bounds:
            // keep its ordinal-named exports.
            let mut grew = false;
            for &i in &ordinal_pending {
                let im = &imgs[i as usize];
                for (k, e) in im.exports.iter().enumerate() {
                    if e.name.is_none() && seen.insert(Node::Export(i, k as u32)) {
                        q.push_back(Node::Export(i, k as u32));
                        grew = true;
                    }
                }
            }
            ordinal_pending.clear();
            if grew {
                continue;
            }
            break;
        };
        let (i, im) = match n {
            Node::Func(i, _) | Node::Region(i, _) | Node::Word(i, _) | Node::Export(i, _) | Node::Slot(i, _) => (i, &imgs[i as usize]),
        };
        let mut follow = |q: &mut VecDeque<Node>, seen: &mut HashSet<Node>, refs: &[(Ref, Use)]| {
            for &(r, u) in refs {
                match (r, u) {
                    (Ref::Code(f), _) => {
                        if seen.insert(Node::Func(i, f)) {
                            q.push_back(Node::Func(i, f));
                        }
                    }
                    (Ref::Region(k), _) => {
                        if seen.insert(Node::Region(i, k)) {
                            q.push_back(Node::Region(i, k));
                        }
                    }
                    (Ref::Word(a), _) => {
                        if seen.insert(Node::Word(i, a)) {
                            q.push_back(Node::Word(i, a));
                        }
                    }
                }
            }
        };
        match n {
            Node::Func(_, f) => {
                follow(&mut q, &mut seen, &im.func_refs[f as usize]);
                // The unit's own calls out of this function's slot.
                if let Some(&k) = im.slot_of.get(&im.entries[f as usize]) {
                    if seen.insert(Node::Slot(i, k)) {
                        q.push_back(Node::Slot(i, k));
                    }
                }
            }
            Node::Region(_, k) => {
                keeps.regions += 1;
                keeps.region_bytes += im.regions[k as usize].1 - im.regions[k as usize].0;
                follow(&mut q, &mut seen, &im.region_refs[k as usize]);
                // The names a reachable region holds: what a lookup can find.
                for nm in im.region_name[k as usize].clone() {
                    lookup(
                        &nm,
                        imgs,
                        &by_name,
                        &by_module,
                        &mut q,
                        &mut seen,
                        &mut named_modules,
                        &mut ordinal_pending,
                        &mut keeps,
                        |k| &mut k.name_lookup,
                    );
                }
            }
            Node::Word(_, a) => {
                // The import this slot binds to, and the pointer it holds.
                if let Some(&k) = im.iat.get(&a) {
                    let imp = &im.imports[k as usize];
                    if let Some(&j) = by_module.get(imp.module.as_str()) {
                        let other = &imgs[j as usize];
                        let e = match (&imp.name, imp.ordinal) {
                            (Some(n), _) => other.by_name.get(n.as_str()).copied(),
                            (None, Some(o)) => other.by_ordinal.get(&o).copied(),
                            _ => None,
                        };
                        if let Some(e) = e {
                            if seen.insert(Node::Export(j, e)) {
                                q.push_back(Node::Export(j, e));
                                keeps.import_bound += 1;
                            }
                        }
                    }
                }
                if let Some(rs) = im.word_ref.get(&a) {
                    follow(&mut q, &mut seen, rs);
                }
            }
            Node::Export(_, e) => {
                let ex = &im.exports[e as usize];
                if let Some(a) = ex.addr {
                    for r in im.at(a) {
                        follow(&mut q, &mut seen, &[(r, Use::Object)]);
                    }
                }
                if let Some(f) = &ex.forwarder {
                    // "OTHER.Name" or "OTHER.#13".
                    if let Some((m, rest)) = f.rsplit_once('.') {
                        let low = m.to_ascii_lowercase();
                        for cand in [format!("{low}.dll"), low.clone()] {
                            if let Some(&j) = by_module.get(cand.as_str()) {
                                let other = &imgs[j as usize];
                                let t = match rest.strip_prefix('#').and_then(|o| o.parse::<u32>().ok()) {
                                    Some(o) => other.by_ordinal.get(&o).copied(),
                                    None => other.by_name.get(rest).copied(),
                                };
                                if let Some(t) = t {
                                    if seen.insert(Node::Export(j, t)) {
                                        q.push_back(Node::Export(j, t));
                                    }
                                }
                                break;
                            }
                        }
                    }
                }
            }
            Node::Slot(_, k) => {
                for &c in &im.wasm_calls[k as usize] {
                    let a = im.slot_addr[c as usize];
                    let hs = im.holders(a);
                    for &f in &hs {
                        if seen.insert(Node::Func(i, f)) {
                            q.push_back(Node::Func(i, f));
                        }
                    }
                    if hs.is_empty() && seen.insert(Node::Slot(i, c)) {
                        q.push_back(Node::Slot(i, c));
                    }
                }
            }
        }
    }

    // GEX_FANOUT_WHY=mspaint:0x101f4af,mspaint:0x10024e4 -- what the closure
    // made of an address: the function or region holding it, and whether it
    // was reached.
    if let Ok(spec) = std::env::var("GEX_FANOUT_WHY") {
        for item in spec.split(',').filter(|s| !s.is_empty()) {
            let Some((m, a)) = item.rsplit_once(':') else { continue };
            let m = m.trim_start_matches('^');
            let Ok(addr) = u64::from_str_radix(a.trim().trim_start_matches("0x"), 16) else { continue };
            let Some((i, im)) = imgs.iter().enumerate().find(|(_, im)| im.module.contains(m)) else {
                eprintln!("[why] {item}: no image named {m}");
                continue;
            };
            let i = i as u32;
            let what: Vec<String> = im
                .at(addr)
                .into_iter()
                .map(|r| match r {
                    Ref::Code(f) => format!(
                        "function {} at {:#x}, reached={}",
                        f,
                        im.entries[f as usize],
                        seen.contains(&Node::Func(i, f))
                    ),
                    Ref::Region(k) => format!(
                        "region {k} [{:#x}, {:#x}), reached={}, {} refs, names {:?}",
                        im.regions[k as usize].0,
                        im.regions[k as usize].1,
                        seen.contains(&Node::Region(i, k)),
                        im.region_refs[k as usize].len(),
                        im.region_name[k as usize]
                    ),
                    Ref::Word(w) => format!("word {w:#x}, reached={}", seen.contains(&Node::Word(i, w))),
                })
                .collect();
            eprintln!("[why] {}:{addr:#x}: {}", im.module, if what.is_empty() { "nothing covers it".into() } else { what.join("; ") });
            // Walk back through everything that refers to it -- functions
            // and data regions alike -- until something the closure reached
            // turns up. That edge is the rule that is wrong; without it the
            // walk names the whole island the run entered and the proof did
            // not.
            if item.starts_with('^') || spec.starts_with('^') {
                let (by_target, _) = im.back_index();
                // Which relocation targets fall in each region, for walking
                // back into one.
                let mut q: VecDeque<(Ref, usize)> = VecDeque::new();
                let mut seen_n: HashSet<Ref> = HashSet::new();
                for r in im.at(addr) {
                    if seen_n.insert(r) {
                        q.push_back((r, 0));
                    }
                }
                let mut found = false;
                let mut sources: Vec<Ref> = Vec::new();
                let (mut funcs, mut regions) = (0usize, 0usize);
                while let Some((n, d)) = q.pop_front() {
                    // The addresses whose pointers would reach this node.
                    let addrs: Vec<u64> = match n {
                        Ref::Code(f) => {
                            funcs += 1;
                            std::iter::once(im.entries[f as usize]).collect()
                        }
                        Ref::Region(r) => {
                            regions += 1;
                            let (s, e) = im.regions[r as usize];
                            by_target.keys().copied().filter(|&t| t >= s && t < e).collect()
                        }
                        Ref::Word(_) => continue,
                    };
                    let mut up: Vec<Ref> = Vec::new();
                    for a in addrs {
                        for &site in by_target.get(&a).map_or(&[][..], |v| v) {
                            match im.region_at(site) {
                                Some(r) => up.push(Ref::Region(r)),
                                None => up.extend(im.holders(site).into_iter().map(Ref::Code)),
                            }
                        }
                    }
                    if let Ref::Code(f) = n {
                        up.extend(
                            (0..im.func_refs.len() as u32)
                                .filter(|&g| im.func_refs[g as usize].iter().any(|&(r, _)| r == Ref::Code(f)))
                                .map(Ref::Code),
                        );
                    }
                    if up.is_empty() {
                        sources.push(n);
                    }
                    for u in up {
                        let hit = match u {
                            Ref::Code(f) => seen.contains(&Node::Func(i, f)),
                            Ref::Region(r) => seen.contains(&Node::Region(i, r)),
                            Ref::Word(a) => seen.contains(&Node::Word(i, a)),
                        };
                        if hit {
                            eprintln!("[why]   REACHED {u:?} refers to it, {d} step(s) back");
                            found = true;
                            break;
                        }
                        if seen_n.insert(u) && d < 40 && seen_n.len() < 20000 {
                            q.push_back((u, d + 1));
                        }
                    }
                    if found {
                        break;
                    }
                }
                if !found {
                    eprintln!(
                        "[why]   an island: nothing reached refers to it, through {funcs} function(s) and {regions} region(s)"
                    );
                    for n in sources.iter().take(10) {
                        let a = match *n {
                            Ref::Code(f) => im.entries[f as usize],
                            Ref::Region(r) => im.regions[r as usize].0,
                            Ref::Word(w) => w,
                        };
                        let ex = im.exports.iter().find(|e| e.addr == Some(a)).map(|e| {
                            e.name.clone().unwrap_or_else(|| format!("#{}", e.ordinal))
                        });
                        eprintln!(
                            "[why]     nothing refers to {n:?} at {a:#x}{}",
                            ex.map(|n| format!(" (export {n})")).unwrap_or_default()
                        );
                    }
                }
            }
        }
    }

    if std::env::var_os("GEX_FANOUT_TRACE").is_some() {
        for (i, im) in imgs.iter().enumerate() {
            let i = i as u32;
            let e = im.entry.unwrap_or(0);
            let hs = im.at(e);
            let entry_reached = hs.iter().any(|r| match r {
                Ref::Code(f) => seen.contains(&Node::Func(i, *f)),
                Ref::Region(r) => seen.contains(&Node::Region(i, *r)),
                Ref::Word(a) => seen.contains(&Node::Word(i, *a)),
            });
            let nf = (0..im.entries.len() as u32).filter(|&f| seen.contains(&Node::Func(i, f))).count();
            let nr = (0..im.regions.len() as u32).filter(|&r| seen.contains(&Node::Region(i, r))).count();
            // How the unit's slots line up with this recovery: a slot that
            // is not a function entry here means the two recoveries differ,
            // and every mapping from a slot to a function is suspect.
            let (mut as_entry, mut as_interior, mut nowhere) = (0usize, 0usize, 0usize);
            for &a in &im.slot_addr {
                if im.at_entry.contains_key(&a) {
                    as_entry += 1;
                } else if !im.holders(a).is_empty() {
                    as_interior += 1;
                } else {
                    nowhere += 1;
                }
            }
            eprintln!(
                "[fanout] {}: base {:#x} entry {e:#x} at={:?} reached={entry_reached}; {nf}/{} functions, {nr}/{} regions, {} relocs; \
                 slots {}: {as_entry} are a function entry, {as_interior} inside one, {nowhere} nowhere",
                im.module, im.base, hs.len(), im.entries.len(), im.regions.len(), im.relocs.len(), im.slot_addr.len()
            );
        }
    }

    // ---- what to keep ----------------------------------------------------
    let mut out = Vec::new();
    for (i, im) in imgs.iter().enumerate() {
        let i = i as u32;
        let mut keep: HashSet<u32> = HashSet::new();
        if im.whole.is_some() {
            keep.extend(0..im.slot_addr.len() as u32);
            out.push(Kept {
                path: im.path.clone(),
                module: im.module.clone(),
                kept: keep.len(),
                keep,
                slots: im.slot_addr.len(),
                functions: 0,
                reached: 0,
                whole: im.whole.clone(),
                exports: im.exports.len(),
                exports_kept: 0,
                reached_funcs: HashSet::new(),
                reached_regions: HashSet::new(),
            });
            continue;
        }
        let reached: HashSet<u32> = (0..im.entries.len() as u32).filter(|&f| seen.contains(&Node::Func(i, f))).collect();
        for (k, &a) in im.slot_addr.iter().enumerate() {
            let hs = im.holders(a);
            if hs.is_empty() || hs.iter().any(|f| reached.contains(f)) {
                keep.insert(k as u32);
            }
        }
        let exports_kept = (0..im.exports.len() as u32).filter(|&e| seen.contains(&Node::Export(i, e))).count();
        out.push(Kept {
            path: im.path.clone(),
            module: im.module.clone(),
            kept: keep.len(),
            keep,
            slots: im.slot_addr.len(),
            functions: im.entries.len(),
            reached: reached.len(),
            whole: None,
            exports: im.exports.len(),
            exports_kept,
            reached_regions: (0..im.regions.len() as u32).filter(|&r| seen.contains(&Node::Region(i, r))).collect(),
            reached_funcs: reached,
        });
    }
    keeps.unresolved = imgs.iter().map(|i| i.unresolved).sum();
    (out, keeps)
}

/// Summarise every unit's image, run the closure, and prune each unit.
pub struct Pruned {
    pub key: String,
    pub path: String,
    pub wasm: Vec<u8>,
    pub kept: Kept,
    pub bytes_before: usize,
    pub bytes_after: usize,
}

/// The closure's answer, with the images it was proved over.
pub struct Analysis {
    pub imgs: Vec<Image>,
    pub kept: Vec<Kept>,
    pub keeps: Keeps,
}

/// Summarise every image and run the closure: `(key, path, unit wasm, image
/// bytes)` for every unit of a site, and the names the site's own native
/// code holds ([`names_in`]).
pub fn analyse(jobs: &[(String, String, Vec<u8>, Vec<u8>)], native: &HashSet<String>, threads: usize) -> Result<Analysis> {
    let units: Vec<gex_split::Unit> = jobs
        .iter()
        .map(|(key, _, wasm, _)| gex_split::Unit::parse(wasm).with_context(|| format!("unit {key}")))
        .collect::<Result<_>>()?;
    let next = std::sync::atomic::AtomicUsize::new(0);
    let slots: Vec<std::sync::Mutex<Option<Result<Image>>>> = jobs.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|sc| {
        for _ in 0..threads.max(1) {
            sc.spawn(|| loop {
                let k = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some((_, path, _, image)) = jobs.get(k) else { break };
                let r = summarize(&units[k], image, path).with_context(|| format!("summarising {path}"));
                *slots[k].lock().unwrap() = Some(r);
            });
        }
    });
    let imgs: Vec<Image> = slots
        .into_iter()
        .map(|m| m.into_inner().unwrap().unwrap())
        .collect::<Result<_>>()?;
    let (kept, keeps) = closure(&imgs, native);
    Ok(Analysis { imgs, kept, keeps })
}

/// The same, then each unit pruned to what the closure kept.
pub fn run(jobs: Vec<(String, String, Vec<u8>, Vec<u8>)>, native: &HashSet<String>, threads: usize) -> Result<(Vec<Pruned>, Keeps)> {
    let a = analyse(&jobs, native, threads)?;
    let (kept, keeps) = (a.kept, a.keeps);
    let mut out = Vec::new();
    for ((key, path, wasm, _), ke) in jobs.iter().zip(kept) {
        let pruned = if ke.keep.len() == ke.slots {
            wasm.clone()
        } else {
            gex_split::prune(wasm, &ke.keep)?
        };
        out.push(Pruned {
            key: key.clone(),
            path: path.clone(),
            bytes_before: wasm.len(),
            bytes_after: pruned.len(),
            wasm: pruned,
            kept: ke,
        });
    }
    Ok((out, keeps))
}

/// The report, as text.
pub fn report(out: &[Pruned], keeps: &Keeps) -> String {
    let mut s = String::new();
    let mut rows: Vec<&Pruned> = out.iter().collect();
    rows.sort_by_key(|p| std::cmp::Reverse(p.kept.slots));
    s.push_str(&format!(
        "{:<36} {:>7} {:>7} {:>7} {:>6}  {:>9}  {:>11} {:>11}\n",
        "image", "slots", "kept", "pruned", "%", "exports", "bytes", "after"
    ));
    let (mut sl, mut kp, mut bb, mut ba) = (0usize, 0usize, 0usize, 0usize);
    for p in &rows {
        let k = &p.kept;
        sl += k.slots;
        kp += k.kept;
        bb += p.bytes_before;
        ba += p.bytes_after;
        s.push_str(&format!(
            "{:<36} {:>7} {:>7} {:>7} {:>5.1}%  {:>4}/{:<4}  {:>11} {:>11}{}\n",
            k.module,
            k.slots,
            k.kept,
            k.slots - k.kept,
            100.0 * (k.slots - k.kept) as f64 / k.slots.max(1) as f64,
            k.exports_kept,
            k.exports,
            p.bytes_before,
            p.bytes_after,
            k.whole.as_deref().map(|w| format!("  whole: {w}")).unwrap_or_default()
        ));
    }
    s.push_str(&format!(
        "\ntotal: {sl} functions, {kp} kept, {} pruned ({:.1}% of functions); \
         units {bb} -> {ba} bytes ({:.1} MB -> {:.1} MB, {:.1} MB smaller)\n",
        sl - kp,
        100.0 * (sl - kp) as f64 / sl.max(1) as f64,
        bb as f64 / 1e6,
        ba as f64 / 1e6,
        (bb - ba) as f64 / 1e6,
    ));
    s.push_str(&keeps_text(keeps));
    s
}

/// The conservative keeps, counted by kind.
pub fn keeps_text(keeps: &Keeps) -> String {
    let mut s = String::new();
    s.push_str("conservative keeps, by kind:\n");
    s.push_str(&format!("  exports kept for a reachable import slot:          {}\n", keeps.import_bound));
    s.push_str(&format!("  exports kept for a name a reachable address holds: {}\n", keeps.name_lookup));
    s.push_str(&format!("  exports kept for a name the site's native code holds: {}\n", keeps.native_lookup));
    if !keeps.unimported.is_empty() {
        s.push_str(&format!(
            "  images nothing imports, so every export is a root:  {} ({} exports)\n",
            keeps.unimported.len(),
            keeps.unimported.iter().map(|(_, n)| n).sum::<usize>()
        ));
    }
    s.push_str(&format!("  slots recovery does not account for:               {}\n", keeps.unknown_slots));
    s.push_str(&format!(
        "  data regions reached:                              {} ({} bytes)\n",
        keeps.regions, keeps.region_bytes
    ));
    s.push_str(&format!("  unresolved indirect branches (sound; watched):      {}\n", keeps.unresolved));
    if !keeps.ordinal_lookup.is_empty() {
        s.push_str("  images left open to an ordinal lookup:\n");
        for (p, n) in &keeps.ordinal_lookup {
            s.push_str(&format!("    {p}: {n} ordinal-named exports\n"));
        }
    }
    if !keeps.whole.is_empty() {
        s.push_str("  images kept whole:\n");
        for (p, w) in &keeps.whole {
            s.push_str(&format!("    {p}: {w}\n"));
        }
    }
    s
}

/// The same counts, as the JSON `build/prune.json` carries.
pub fn keeps_json(keeps: &Keeps) -> String {
    use crate::json::{n, obj, s as js, Json};
    crate::json::write(&obj(vec![
        ("import_bound", n(keeps.import_bound as u64)),
        ("native_lookup", n(keeps.native_lookup as u64)),
        (
            "unimported",
            Json::Arr(keeps.unimported.iter().map(|(p, k)| obj(vec![("path", js(p)), ("exports", n(*k as u64))])).collect()),
        ),
        ("name_lookup", n(keeps.name_lookup as u64)),
        ("unknown_slots", n(keeps.unknown_slots as u64)),
        ("regions", n(keeps.regions as u64)),
        ("region_bytes", n(keeps.region_bytes)),
        ("unresolved", n(keeps.unresolved as u64)),
        (
            "ordinal_lookup",
            Json::Arr(
                keeps
                    .ordinal_lookup
                    .iter()
                    .map(|(p, k)| obj(vec![("path", js(p)), ("exports", n(*k as u64))]))
                    .collect(),
            ),
        ),
        (
            "whole",
            Json::Arr(
                keeps
                    .whole
                    .iter()
                    .map(|(p, w)| obj(vec![("path", js(p)), ("why", js(w))]))
                    .collect(),
            ),
        ),
    ]))
}

/// Run the analysis over an ejected project's units and images, without
/// building the site: `gexc eject prune <project>`. Returns the pruned units
/// and the report's counts.
pub fn project(dir: &std::path::Path) -> Result<(Vec<Pruned>, Keeps)> {
    let (jobs, native) = inputs(dir)?;
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
    run(jobs, &native, threads)
}

/// The names the site's own native code holds: every module the recording
/// loaded as native wasm (the Wine core, its loaders), from the bundle.
pub fn native_names(project: &crate::json::Json, tar: &crate::reach::Tar) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    for m in project.get("modules").and_then(|x| x.as_arr()).unwrap_or(&[]) {
        let p = crate::json::str_field(m, "path", "gex-project.json modules")?;
        let guest = p.split_once(':').map(|(_, r)| r).unwrap_or(&p);
        let Some(bytes) = tar.read(guest) else { continue };
        out.extend(names_in(bytes));
    }
    Ok(out)
}

/// Check the closure against a run: every function a real run called must be
/// one the proof keeps.
///
/// A site built with `"cold": "instrument"` has every function report itself
/// on entry, which gex.mjs flushes as `[gex-cov] <key> <slot or a-b>,...`.
/// Given a console log of such a run, this says which functions ran that the
/// proof would have left out -- each one a bug in a rule here, found without
/// a pruned build and a trap.
pub fn check(dir: &std::path::Path, log: &str) -> Result<String> {
    let (jobs, native) = inputs(dir)?;
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
    let a = analyse(&jobs, &native, threads)?;
    // key -> the slots the run entered.
    let mut ran: HashMap<&str, HashSet<u32>> = HashMap::new();
    for line in log.lines() {
        let Some(rest) = line.split("[gex-cov] ").nth(1) else { continue };
        let mut it = rest.split_whitespace();
        let Some(key) = it.next() else { continue };
        let Some(list) = it.next() else { continue };
        let set = ran.entry(key).or_default();
        for part in list.split(',') {
            match part.split_once('-') {
                Some((a, b)) => {
                    if let (Ok(a), Ok(b)) = (a.parse::<u32>(), b.parse::<u32>()) {
                        for k in a..=b.min(a.saturating_add(1 << 20)) {
                            set.insert(k);
                        }
                    }
                }
                None => {
                    if let Ok(k) = part.parse::<u32>() {
                        set.insert(k);
                    }
                }
            }
        }
    }
    let mut s = String::new();
    if ran.is_empty() {
        s.push_str("no [gex-cov] lines in the log: was the site built with \"cold\": \"instrument\"?\n");
        return Ok(s);
    }
    let (mut covered, mut missed_all, mut frontier_all) = (0usize, 0usize, 0usize);
    let mut run_roots: Vec<(String, Vec<u64>)> = Vec::new();
    let mut rows: Vec<(usize, String)> = Vec::new();
    for ((key, path, _, _), ke) in jobs.iter().zip(&a.kept) {
        let Some(hit) = ran.get(key.as_str()) else { continue };
        let img = a.imgs.iter().find(|i| i.path == *path);
        let missed: Vec<u32> = hit.iter().copied().filter(|k| !ke.keep.contains(k)).collect();
        covered += hit.len();
        missed_all += missed.len();
        if missed.is_empty() {
            continue;
        }
        let mut addrs: Vec<String> = missed
            .iter()
            .take(12)
            .map(|&k| {
                img.and_then(|i| i.slot_addrs().get(k as usize))
                    .map_or_else(|| format!("slot {k}"), |a| format!("{a:#x}"))
            })
            .collect();
        if missed.len() > addrs.len() {
            addrs.push(format!("+{} more", missed.len() - addrs.len()));
        }
        // The frontier: the ones something reached points at. The rest
        // follow from those, so fixing a frontier miss fixes a crowd.
        let ix = img.map(|i| i.back_index());
        // The run's own entry points into this image: functions that ran
        // which nothing that ran refers to. Each is something outside the
        // image called, so each is a root the proof has to explain.
        if let (Some(i), Some(ix)) = (img, &ix) {
            let covered_funcs: HashSet<u32> = hit
                .iter()
                .filter_map(|&k| i.slot_addrs().get(k as usize))
                .filter_map(|&a| match i.at(a).first() {
                    Some(&Ref::Code(f)) => Some(f),
                    _ => None,
                })
                .collect();
            let mut roots: Vec<u64> = Vec::new();
            for &k in missed.iter() {
                let Some(&a) = i.slot_addrs().get(k as usize) else { continue };
                let from_data = ix.0.get(&a).map_or(false, |sites| {
                    sites.iter().any(|&s| i.holders(s).iter().any(|f| covered_funcs.contains(f)))
                });
                let from_call = match i.at(a).first() {
                    Some(&Ref::Code(f)) => ix.1.get(&f).is_some_and(|gs| gs.iter().any(|g| covered_funcs.contains(g))),
                    _ => false,
                };
                if !from_data && !from_call {
                    roots.push(a);
                }
            }
            roots.sort_unstable();
            run_roots.push((ke.module.clone(), roots));
        }
        let front: Vec<u32> = match (img, &ix) {
            (Some(i), Some(ix)) => missed
                .iter()
                .copied()
                .filter(|&k| i.slot_addrs().get(k as usize).is_some_and(|&a| i.frontier(a, ke, ix)))
                .collect(),
            _ => Vec::new(),
        };
        frontier_all += front.len();
        let mut row = format!(
            "{:<30} {:>6} ran, {:>5} would be pruned, {:>4} of them at the frontier: {}\n",
            ke.module,
            hit.len(),
            missed.len(),
            front.len(),
            addrs.join(" ")
        );
        if let (Some(i), Some(ix)) = (img, &ix) {
            for &k in front.iter().take(6) {
                if let Some(&a) = i.slot_addrs().get(k as usize) {
                    row.push_str(&format!("    {a:#x}: {}\n", i.why(a, ke, ix)));
                }
            }
        }
        rows.push((missed.len(), row));
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    for (_, r) in &rows {
        s.push_str(r);
    }
    s.push_str("\nwhat the run entered each image at that the proof missed:\n");
    for (m, rs) in run_roots.iter().filter(|(_, r)| !r.is_empty()) {
        let shown: Vec<String> = rs.iter().take(10).map(|a| format!("{a:#x}")).collect();
        s.push_str(&format!("  {:<28} {:>5}: {}\n", m, rs.len(), shown.join(" ")));
    }
    s.push_str(&format!(
        "\n{covered} functions ran across {} unit(s); {missed_all} of them the proof would leave out, \
         {frontier_all} at the frontier{}\n",
        ran.len(),
        if missed_all == 0 { " -- sound over this run" } else { "" }
    ));
    Ok(s)
}

/// A project's units and the names its native code holds.
fn inputs(dir: &std::path::Path) -> Result<(Vec<(String, String, Vec<u8>, Vec<u8>)>, HashSet<String>)> {
    use crate::json::Json;
    let pj = dir.join("gex-project.json");
    let text = std::fs::read_to_string(&pj).with_context(|| format!("reading {}", pj.display()))?;
    let v = Json::parse(&text).map_err(|e| anyhow::anyhow!("{}: {e}", pj.display()))?;
    let units = v.get("units").and_then(|x| x.as_arr()).unwrap_or(&[]);
    let tar_path = dir.join("bundle/rootfs.tar");
    let tar_bytes = std::fs::read(&tar_path).with_context(|| format!("reading {}", tar_path.display()))?;
    let tar = crate::reach::Tar::parse(&tar_bytes)?;
    let mut jobs: Vec<(String, String, Vec<u8>, Vec<u8>)> = Vec::new();
    for u in units {
        let key = crate::json::str_field(u, "key", "gex-project.json units")?;
        let path = crate::json::str_field(u, "path", "gex-project.json units")?;
        let wasm = std::fs::read(dir.join("units").join(&key).join("c.wasm"))
            .with_context(|| format!("reading the unit of {path}"))?;
        let image = tar
            .read(&path)
            .ok_or_else(|| anyhow::anyhow!("{path}: not in bundle/rootfs.tar"))?
            .to_vec();
        jobs.push((key, path, wasm, image));
    }
    let native = native_names(&v, &tar)?;
    Ok((jobs, native))
}
