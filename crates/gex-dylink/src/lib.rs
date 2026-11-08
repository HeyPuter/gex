//! Shared libraries for native wasm32 programs.
//!
//! A gexcc shared library is an emscripten-style side module: a wasm module
//! with a `dylink.0` section (its data size and alignment, its table size,
//! the libraries it needs), importing the process's memory, function table
//! and stack pointer, `__memory_base` and `__table_base`, data addresses as
//! `GOT.mem.<sym>` globals, function addresses (table indices) as
//! `GOT.func.<sym>` globals and other modules' functions as `env.<sym>`
//! imports. A program linked against shared libraries is a PIE of the same
//! shape whose `dylink.0` names them.
//!
//! This crate is the half of the loader both shims share: reading a module
//! ([`Info`]), finding and ordering a program's libraries ([`Link`]), and
//! deciding what every import of every module is bound to ([`Link::bind`]).
//! The shims do the rest (data allocation, table ranges, instantiation in
//! each thread's store). [`selfbind`] is a build step gexcc runs on every
//! shared library.
//!
//! Function addresses. Every function a module exports gets a table slot
//! in the module's own range: the functions of its element segment keep
//! their positions, and every other exported function gets one after them
//! ([`Info::slots`]). `GOT.func.f` is then the slot of the module defining
//! `f`, the same in every module, so function pointers compare equal across
//! modules as C requires.

use std::collections::VecDeque;
use std::hash::{BuildHasherDefault, Hasher};

/// The crate's maps and sets, keyed by symbol names and indices: hashed
/// with [`FxHasher`], not std's SipHash, which was most of the loader's time
/// in a process binding a large C++ library's tens of thousands of symbols.
pub type HashMap<K, V> = std::collections::HashMap<K, V, BuildHasherDefault<FxHasher>>;
pub type HashSet<K> = std::collections::HashSet<K, BuildHasherDefault<FxHasher>>;

/// rustc's Fx hash: a rotate, xor and multiply per word. Not resistant to
/// chosen keys, which only a process's own libraries give.
#[derive(Clone, Copy, Default)]
pub struct FxHasher(u64);

impl FxHasher {
    #[inline]
    fn add(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, mut b: &[u8]) {
        while b.len() >= 8 {
            self.add(u64::from_le_bytes(b[..8].try_into().unwrap()));
            b = &b[8..];
        }
        if b.len() >= 4 {
            self.add(u32::from_le_bytes(b[..4].try_into().unwrap()) as u64);
            b = &b[4..];
        }
        for &c in b {
            self.add(c as u64);
        }
    }
    #[inline]
    fn write_u8(&mut self, v: u8) {
        self.add(v as u64);
    }
    #[inline]
    fn write_u32(&mut self, v: u32) {
        self.add(v as u64);
    }
    #[inline]
    fn write_u64(&mut self, v: u64) {
        self.add(v);
    }
    #[inline]
    fn write_usize(&mut self, v: usize) {
        self.add(v as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

pub use anyhow::{anyhow, bail, Context, Result};

/// `dylink.0` symbol flags (llvm/BinaryFormat/Wasm.h).
pub const SYM_BINDING_WEAK: u32 = 0x1;
pub const SYM_TLS: u32 = 0x400;

/// The directories a library is searched in after the program's run path
/// and `LD_LIBRARY_PATH`: the multiarch directories gexcc's packages own.
pub const DEFAULT_DIRS: &[&str] = &[
    "/usr/lib/wasm32-linux-musl",
    "/lib/wasm32-linux-musl",
    "/usr/local/lib/wasm32-linux-musl",
];

/// The custom section a library's fork summary is in: which of its
/// exports may unwind for a fork.
pub const FORKREACH_SECTION: &str = "gex.forkreach";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Func,
    Table,
    Memory,
    Global,
    Tag,
}

#[derive(Clone, Debug)]
pub struct Import {
    pub module: String,
    pub name: String,
    pub kind: Kind,
    /// For a function: its type index; for a global: whether mutable.
    pub ty: u32,
    pub weak: bool,
    /// A global of type `i64` (a wasm64 module's `GOT.*` and bases).
    pub wide: bool,
}

#[derive(Clone, Debug)]
pub struct Export {
    pub kind: Kind,
    /// The index in the kind's index space (imports first).
    pub index: u32,
}

/// A module's exports by name: the names in one buffer, the exports in the
/// section's order, and an open-addressed index of them by the name's hash.
/// A map of a `String` each was a third of a library's parse at every load
/// (Qt's and KDE's libraries export 8.4 MB of names, 225 thousand of them:
/// one allocation and copy each, in every process).
#[derive(Clone, Default)]
pub struct Exports {
    names: String,
    /// (name start, name end, export)
    ents: Vec<(u32, u32, Export)>,
    /// `ents` indices, `u32::MAX` free; a power of two long, at most half full.
    index: Vec<u32>,
}

impl Exports {
    fn with_capacity(n: usize, name_bytes: usize) -> Exports {
        Exports { names: String::with_capacity(name_bytes), ents: Vec::with_capacity(n), index: Vec::new() }
    }

    fn hash(name: &str) -> usize {
        let mut h = FxHasher::default();
        h.write(name.as_bytes());
        h.finish() as usize
    }

    fn name(&self, i: usize) -> &str {
        let (a, b, _) = self.ents[i];
        &self.names[a as usize..b as usize]
    }

    /// Add `name` (last wins, as a map's insert): the index is made again
    /// as it fills.
    pub fn insert(&mut self, name: &str, e: Export) {
        if let Some(i) = self.find(name) {
            self.ents[i].2 = e;
            return;
        }
        let a = self.names.len() as u32;
        self.names.push_str(name);
        self.ents.push((a, self.names.len() as u32, e));
        if self.ents.len() * 2 > self.index.len() {
            self.reindex((self.ents.len() * 2).next_power_of_two().max(16));
        } else {
            let i = self.ents.len() - 1;
            self.place(i);
        }
    }

    fn reindex(&mut self, size: usize) {
        self.index.clear();
        self.index.resize(size, u32::MAX);
        for i in 0..self.ents.len() {
            self.place(i);
        }
    }

    fn place(&mut self, i: usize) {
        let mask = self.index.len() - 1;
        let mut at = Self::hash(self.name(i)) & mask;
        while self.index[at] != u32::MAX {
            at = (at + 1) & mask;
        }
        self.index[at] = i as u32;
    }

    fn find(&self, name: &str) -> Option<usize> {
        if self.index.is_empty() {
            return None;
        }
        let mask = self.index.len() - 1;
        let mut at = Self::hash(name) & mask;
        loop {
            let i = self.index[at];
            if i == u32::MAX {
                return None;
            }
            if self.name(i as usize) == name {
                return Some(i as usize);
            }
            at = (at + 1) & mask;
        }
    }

    pub fn get(&self, name: &str) -> Option<&Export> {
        self.find(name).map(|i| &self.ents[i].2)
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    pub fn len(&self) -> usize {
        self.ents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ents.is_empty()
    }

    /// (name, export), in the section's order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Export)> + '_ {
        self.ents.iter().map(|(a, b, e)| (&self.names[*a as usize..*b as usize], e))
    }
}

impl std::ops::Index<&str> for Exports {
    type Output = Export;
    fn index(&self, name: &str) -> &Export {
        self.get(name).unwrap_or_else(|| panic!("no export {name}"))
    }
}

impl<'a> IntoIterator for &'a Exports {
    type Item = (&'a str, &'a Export);
    type IntoIter = Box<dyn Iterator<Item = (&'a str, &'a Export)> + 'a>;
    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

impl<S: AsRef<str>> FromIterator<(S, Export)> for Exports {
    fn from_iter<I: IntoIterator<Item = (S, Export)>>(it: I) -> Exports {
        let mut x = Exports::default();
        for (n, e) in it {
            x.insert(n.as_ref(), e);
        }
        x
    }
}

impl std::fmt::Debug for Exports {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// What the loader needs to know of a module.
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub mem_size: u32,
    /// log2.
    pub mem_align: u32,
    pub table_size: u32,
    pub table_align: u32,
    pub needed: Vec<String>,
    pub runpath: Vec<String>,
    /// Whether the module has a `dylink.0` section at all.
    pub dylink: bool,
    pub imports: Vec<Import>,
    pub exports: Exports,
    /// Exports flagged TLS in `dylink.0`.
    pub tls_exports: HashSet<String>,
    pub func_imports: u32,
    pub global_imports: u32,
    /// The function at each position of the element segment (based at
    /// `__table_base`).
    pub elem: Vec<u32>,
    /// The constant initial value of each defined global (index minus the
    /// imported globals), if it has one: a data export's address relative
    /// to `__memory_base`.
    pub global_init: Vec<Option<u32>>,
    /// Each type's signature as `params_results`, one letter a value
    /// (`i` i32, `j` i64, `f` f32, `d` f64), as the page writes a function
    /// that calls through the table (`L` in a binding list); `None` for a
    /// type with any other value or a non-function type.
    pub sigs: Vec<Option<String>>,
    /// [`Info::slots`], made once: the loader asks for it at every symbol it
    /// binds (a large C++ library binds tens of thousands).
    slots: std::sync::OnceLock<(u32, HashMap<u32, u32>)>,
    /// Whether any export is an fpcast thunk ([`FPCAST_THUNK`]): only then
    /// does a slot ask for one by name (most modules have none, and the
    /// question made a string per exported function).
    has_thunks: bool,
}

/// A function type as [`Info::sigs`] writes it.
fn sig_text(params: &[wasmparser::ValType], results: &[wasmparser::ValType]) -> Option<String> {
    use wasmparser::ValType;
    let l = |v: &ValType| match v {
        ValType::I32 => Some('i'),
        ValType::I64 => Some('j'),
        ValType::F32 => Some('f'),
        ValType::F64 => Some('d'),
        _ => None,
    };
    let mut out = String::new();
    for v in params {
        out.push(l(v)?);
    }
    out.push('_');
    for v in results {
        out.push(l(v)?);
    }
    Some(out)
}

/// A library's fork summary (`gex.forkreach`).
#[derive(Clone, Debug, Default)]
pub struct ForkReach {
    /// Exports that may unwind (fork, or call something that may).
    pub may_fork: HashSet<String>,
}

impl ForkReach {
    /// The section's payload: UTF-8 text, a first line `gex.forkreach 1`,
    /// then one export name per line.
    pub fn encode(&self) -> Vec<u8> {
        let mut names: Vec<&String> = self.may_fork.iter().collect();
        names.sort();
        let mut s = String::from("gex.forkreach 1\n");
        for n in names {
            s.push_str(n);
            s.push('\n');
        }
        s.into_bytes()
    }

    /// `wasm`'s summary, if it has one. Only gexcc's link reads it (a
    /// loader has no use for it: [`Info`] leaves it out, which was a third
    /// of a library's parse, 7.6 MB of names across KDE's).
    pub fn of(wasm: &[u8]) -> Result<Option<ForkReach>> {
        for p in wasmparser::Parser::new(0).parse_all(wasm) {
            if let wasmparser::Payload::CustomSection(c) = p? {
                if c.name() == FORKREACH_SECTION {
                    return Ok(Some(ForkReach::decode(c.data())?));
                }
            }
        }
        Ok(None)
    }

    pub fn decode(data: &[u8]) -> Result<ForkReach> {
        let s = std::str::from_utf8(data).context("gex.forkreach: not UTF-8")?;
        let mut lines = s.lines();
        if lines.next() != Some("gex.forkreach 1") {
            bail!("gex.forkreach: unknown version");
        }
        Ok(ForkReach {
            may_fork: lines
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect(),
        })
    }
}

fn kind_of(k: wasmparser::ExternalKind) -> Kind {
    match k {
        wasmparser::ExternalKind::Func => Kind::Func,
        wasmparser::ExternalKind::Table => Kind::Table,
        wasmparser::ExternalKind::Memory => Kind::Memory,
        wasmparser::ExternalKind::Global => Kind::Global,
        wasmparser::ExternalKind::Tag => Kind::Tag,
    }
}

impl Info {
    /// Whether [`Info::parse`] reads a section (by id, and a custom
    /// section's name): a module of only these parses to the same `Info`.
    /// A loader that has the module compiled already reads no more of the
    /// file (its code and data are nineteen twentieths of a Qt library).
    pub fn reads_section(id: u8, name: &[u8]) -> bool {
        match id {
            0 => name == b"dylink.0",
            // types, imports, globals, exports, elements
            1 | 2 | 6 | 7 | 9 => true,
            _ => false,
        }
    }

    pub fn parse(wasm: &[u8]) -> Result<Info> {
        use wasmparser::{Payload, TypeRef};
        let mut info = Info::default();
        // `module\x01field` of each weak import, looked up through one buffer
        let mut weak: HashSet<String> = HashSet::default();
        for payload in wasmparser::Parser::new(0).parse_all(wasm) {
            match payload? {
                Payload::CustomSection(c) if c.name() == "dylink.0" => {
                    info.dylink = true;
                    let mut r = wasmparser::BinaryReader::new(c.data(), 0);
                    while !r.eof() {
                        let id = r.read_u8()?;
                        let len = r.read_var_u32()? as usize;
                        let body = r.read_bytes(len)?;
                        let mut s = wasmparser::BinaryReader::new(body, 0);
                        match id {
                            1 => {
                                info.mem_size = s.read_var_u32()?;
                                info.mem_align = s.read_var_u32()?;
                                info.table_size = s.read_var_u32()?;
                                info.table_align = s.read_var_u32()?;
                            }
                            2 | 5 => {
                                let n = s.read_var_u32()?;
                                for _ in 0..n {
                                    let v = s.read_string()?.to_string();
                                    if id == 2 {
                                        info.needed.push(v)
                                    } else {
                                        info.runpath.push(v)
                                    }
                                }
                            }
                            3 => {
                                let n = s.read_var_u32()?;
                                for _ in 0..n {
                                    let name = s.read_string()?.to_string();
                                    let flags = s.read_var_u32()?;
                                    if flags & SYM_TLS != 0 {
                                        info.tls_exports.insert(name);
                                    }
                                }
                            }
                            4 => {
                                let n = s.read_var_u32()?;
                                for _ in 0..n {
                                    let m = s.read_string()?;
                                    let f = s.read_string()?;
                                    let flags = s.read_var_u32()?;
                                    if flags & SYM_BINDING_WEAK != 0 {
                                        weak.insert(format!("{m}\x01{f}"));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Payload::TypeSection(r) => {
                    for g in r {
                        for t in g?.into_types() {
                            info.sigs.push(match &t.composite_type.inner {
                                wasmparser::CompositeInnerType::Func(f) => sig_text(f.params(), f.results()),
                                _ => None,
                            });
                        }
                    }
                }
                Payload::ImportSection(s) => {
                    info.imports.reserve(s.count() as usize);
                    for imp in s {
                        let imp = imp?;
                        let wide = matches!(imp.ty, TypeRef::Global(g) if g.content_type == wasmparser::ValType::I64);
                        let (kind, ty) = match imp.ty {
                            TypeRef::Func(t) => {
                                info.func_imports += 1;
                                (Kind::Func, t)
                            }
                            TypeRef::Global(g) => {
                                info.global_imports += 1;
                                (Kind::Global, g.mutable as u32)
                            }
                            TypeRef::Table(_) => (Kind::Table, 0),
                            TypeRef::Memory(_) => (Kind::Memory, 0),
                            TypeRef::Tag(t) => (Kind::Tag, t.func_type_idx),
                        };
                        info.imports.push(Import {
                            module: imp.module.to_string(),
                            name: imp.name.to_string(),
                            kind,
                            ty,
                            weak: false,
                            wide,
                        });
                    }
                }
                Payload::GlobalSection(s) => {
                    for g in s {
                        let g = g?;
                        let mut ops = g.init_expr.get_operators_reader();
                        let v = match ops.read() {
                            Ok(wasmparser::Operator::I32Const { value }) => Some(value as u32),
                            // a wasm64 program lowered to wasm32 (its addresses
                            // below 4 GiB)
                            Ok(wasmparser::Operator::I64Const { value }) => u32::try_from(value).ok(),
                            _ => None,
                        };
                        info.global_init.push(v);
                    }
                }
                Payload::ExportSection(s) => {
                    // (the section's bytes bound its names')
                    let mut x = Exports::with_capacity(s.count() as usize, s.range().len());
                    for e in s {
                        let e = e?;
                        info.has_thunks |= e.name.starts_with(FPCAST_THUNK);
                        let a = x.names.len() as u32;
                        x.names.push_str(e.name);
                        x.ents.push((a, x.names.len() as u32, Export { kind: kind_of(e.kind), index: e.index }));
                    }
                    // a module's export names are distinct (the format says so)
                    x.reindex((x.ents.len() * 2).next_power_of_two().max(16));
                    info.exports = x;
                }
                Payload::ElementSection(s) => {
                    for el in s {
                        let el = el?;
                        if !matches!(el.kind, wasmparser::ElementKind::Active { .. }) {
                            continue;
                        }
                        if let wasmparser::ElementItems::Functions(fs) = el.items {
                            for f in fs {
                                info.elem.push(f?);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if !weak.is_empty() {
            let mut key = String::new();
            let mut is_weak = |m: &str, f: &str| {
                key.clear();
                key.push_str(m);
                key.push('\x01');
                key.push_str(f);
                weak.contains(&key)
            };
            for imp in &mut info.imports {
                // `GOT.mem.x` / `GOT.func.x` carry `env.x`'s weakness
                imp.weak = is_weak(&imp.module, &imp.name) || is_weak("env", &imp.name);
            }
        }
        Ok(info)
    }

    /// The module's table range: its length, and the slot (relative to
    /// `__table_base`) of every exported function. Element segment
    /// positions first; every exported function not in it gets a slot
    /// after them, in export-name order (the same in every process).
    pub fn slots(&self) -> (u32, &HashMap<u32, u32>) {
        let (len, at) = self.slots.get_or_init(|| self.make_slots());
        (*len, at)
    }

    fn make_slots(&self) -> (u32, HashMap<u32, u32>) {
        let mut at: HashMap<u32, u32> = HashMap::default();
        for (k, &f) in self.elem.iter().enumerate() {
            at.entry(f).or_insert(k as u32);
        }
        let mut len = self.table_size.max(self.elem.len() as u32);
        let mut names: Vec<(&str, &Export)> =
            self.exports.iter().filter(|(n, e)| e.kind == Kind::Func && !n.starts_with(FPCAST_THUNK)).collect();
        names.sort_by(|a, b| a.0.cmp(b.0));
        for (n, e) in names {
            // a function pointer to an export of an fpcast module is its
            // thunk's slot (fpcast_post)
            let target = self.table_target(n).unwrap_or(e.index);
            let slot = match at.get(&target) {
                Some(&s) => s,
                None => {
                    at.insert(target, len);
                    len += 1;
                    len - 1
                }
            };
            at.entry(e.index).or_insert(slot);
        }
        (len, at)
    }

    /// The function a table slot for export `name` holds: its fpcast thunk
    /// when the module has one, else the function itself.
    pub fn table_target(&self, name: &str) -> Option<u32> {
        let e = self.exports.get(name)?;
        if !self.has_thunks {
            return Some(e.index);
        }
        Some(self.exports.get(&format!("{FPCAST_THUNK}{name}")).map_or(e.index, |t| t.index))
    }

    /// The slots [`Info::slots`] adds after the element segment: (relative
    /// slot, export to fill it with), which the loader fills after
    /// instantiating.
    pub fn extra_slots(&self) -> Vec<(u32, String)> {
        let (_, at) = self.slots();
        let base = self.table_size.max(self.elem.len() as u32);
        let mut v: Vec<(u32, String)> = self
            .exports
            .iter()
            .filter(|(n, e)| e.kind == Kind::Func && !n.starts_with(FPCAST_THUNK))
            .filter_map(|(n, e)| {
                let thunk = if self.has_thunks { format!("{FPCAST_THUNK}{n}") } else { String::new() };
                let fill = if self.has_thunks && self.exports.contains_key(&thunk) { thunk } else { n.to_string() };
                at.get(&e.index).filter(|&&s| s >= base).map(|&s| (s, fill))
            })
            .collect();
        v.sort();
        v.dedup_by_key(|x| x.0);
        v
    }

    /// A static program's undefined weak symbols, as ELF leaves them: a
    /// `GOT.func` or `GOT.mem` import (the symbol's address) is 0, and a
    /// weak function the program calls directly is [`Bind::Missing`] (a
    /// trap if the null check is skipped). wasm-ld resolves everything else
    /// in a static link, so any other `GOT.*` import is a strong undefined
    /// symbol, an error here as a link error would have been.
    /// `(module, name, binding)` per import it concerns.
    /// Each with whether it is an `i64` global.
    pub fn static_weak_binds(&self) -> std::result::Result<Vec<(String, String, Bind, bool)>, String> {
        let mut out = Vec::new();
        for imp in &self.imports {
            match imp.module.as_str() {
                "GOT.func" | "GOT.mem" if imp.weak => {
                    let b = if imp.module == "GOT.func" { Bind::GotFunc(0) } else { Bind::GotMem(0) };
                    out.push((imp.module.clone(), imp.name.clone(), b, imp.wide));
                }
                "GOT.func" | "GOT.mem" => return Err(format!("undefined symbol: {}", imp.name)),
                "env" if imp.weak && imp.kind == Kind::Func => {
                    out.push((imp.module.clone(), imp.name.clone(), Bind::Missing(String::new()), false));
                }
                _ => {}
            }
        }
        Ok(out)
    }

    /// A data export's address relative to `__memory_base`.
    pub fn data_offset(&self, name: &str) -> Option<u32> {
        let e = self.exports.get(name)?;
        if e.kind != Kind::Global || e.index < self.global_imports {
            return None;
        }
        self.global_init
            .get((e.index - self.global_imports) as usize)
            .copied()
            .flatten()
    }
}

/// Rewrite a side module's imports of functions it defines itself into
/// direct calls. wasm-ld makes every weak definition of a shared library
/// preemptible -- called through an `env` import even with `-Bsymbolic` --
/// and musl's weak aliases (`malloc`, `lseek`, the `*_l` functions) and
/// every C++ inline function are weak. gex's loader does not interpose a
/// library's own calls, so they are bound here, once,
/// at build time. Returns the module unchanged when nothing is imported
/// that it defines.
pub fn selfbind(wasm: &[u8]) -> Result<Vec<u8>> {
    use wasm_encoder::reencode::{Reencode, RoundtripReencoder};
    let info = Info::parse(wasm)?;
    // imported function index -> the defined function it becomes
    let mut bound: HashMap<u32, u32> = HashMap::default();
    let mut fi = 0u32;
    for imp in &info.imports {
        if imp.kind != Kind::Func {
            continue;
        }
        if imp.module == "env" {
            if let Some(e) = info.exports.get(&imp.name) {
                if e.kind == Kind::Func && e.index >= info.func_imports {
                    bound.insert(fi, e.index);
                }
            }
        }
        fi += 1;
    }
    if bound.is_empty() {
        return Ok(wasm.to_vec());
    }
    // old index -> new index
    let n = info.func_imports;
    let removed = bound.len() as u32;
    let mut kept_before = vec![0u32; n as usize + 1];
    for i in 0..n {
        kept_before[i as usize + 1] = kept_before[i as usize] + u32::from(!bound.contains_key(&i));
    }
    struct R {
        n: u32,
        removed: u32,
        bound: HashMap<u32, u32>,
        kept_before: Vec<u32>,
        func_import: u32,
    }
    impl R {
        fn map(&self, f: u32) -> u32 {
            if f >= self.n {
                f - self.removed
            } else if let Some(&d) = self.bound.get(&f) {
                d - self.removed
            } else {
                self.kept_before[f as usize]
            }
        }
    }
    impl Reencode for R {
        type Error = std::convert::Infallible;
        fn function_index(&mut self, f: u32) -> u32 {
            self.map(f)
        }
        fn parse_import(
            &mut self,
            imports: &mut wasm_encoder::ImportSection,
            import: wasmparser::Import<'_>,
        ) -> Result<(), wasm_encoder::reencode::Error<Self::Error>> {
            if let wasmparser::TypeRef::Func(_) = import.ty {
                let i = self.func_import;
                self.func_import += 1;
                if self.bound.contains_key(&i) {
                    return Ok(());
                }
            }
            imports.import(import.module, import.name, self.entity_type(import.ty)?);
            Ok(())
        }
        fn parse_custom_name_subsection(
            &mut self,
            names: &mut wasm_encoder::NameSection,
            section: wasmparser::Name<'_>,
        ) -> Result<(), wasm_encoder::reencode::Error<Self::Error>> {
            // the bound imports' names go (their functions keep their own)
            let keep = |i: u32| i >= self.n || !self.bound.contains_key(&i);
            match section {
                wasmparser::Name::Function(map) => {
                    let mut m = wasm_encoder::NameMap::new();
                    for x in map {
                        let x = x?;
                        if keep(x.index) {
                            m.append(self.map(x.index), x.name);
                        }
                    }
                    names.functions(&m);
                }
                wasmparser::Name::Local(_) | wasmparser::Name::Label(_) => {}
                other => {
                    wasm_encoder::reencode::utils::parse_custom_name_subsection(self, names, other)?
                }
            }
            Ok(())
        }
    }
    let _ = RoundtripReencoder;
    let mut r = R {
        n,
        removed,
        bound,
        kept_before,
        func_import: 0,
    };
    let mut module = wasm_encoder::Module::new();
    r.parse_core_module(&mut module, wasmparser::Parser::new(0), wasm)
        .map_err(|e| anyhow!("selfbind: {e:?}"))?;
    let out = module.finish();
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&out)
        .context("selfbind: the rewritten module does not validate")?;
    Ok(out)
}

/// The custom section [`fpcast_prep`] leaves for [`fpcast_post`]: the
/// exported functions, in the order of the passive segment it added.
const FPCAST_PREP: &str = "gex.fpcast.prep";
/// The prefix of the export naming an exported function's fpcast thunk.
pub const FPCAST_THUNK: &str = "__fpcast$";

/// Before binaryen's `--fpcast-emu`: a passive element segment holding
/// every exported function, so the pass makes each a thunk of the uniform
/// signature (it rewrites what tables can reach), and the list of their
/// names. [`fpcast_post`] exports the thunks.
pub fn fpcast_prep(wasm: &[u8]) -> Result<Vec<u8>> {
    use wasm_encoder::reencode::Reencode;
    let info = Info::parse(wasm)?;
    let mut names: Vec<(&str, u32)> = info
        .exports
        .iter()
        .filter(|(n, e)| e.kind == Kind::Func && e.index >= info.func_imports && !n.starts_with(FPCAST_THUNK))
        .map(|(n, e)| (n, e.index))
        .collect();
    names.sort();
    struct R {
        funcs: Vec<u32>,
        done: bool,
    }
    impl Reencode for R {
        type Error = std::convert::Infallible;
        fn parse_element_section(
            &mut self,
            elements: &mut wasm_encoder::ElementSection,
            section: wasmparser::ElementSectionReader<'_>,
        ) -> Result<(), wasm_encoder::reencode::Error<Self::Error>> {
            wasm_encoder::reencode::utils::parse_element_section(self, elements, section)?;
            elements.passive(wasm_encoder::Elements::Functions(self.funcs.clone().into()));
            self.done = true;
            Ok(())
        }
    }
    let mut r = R { funcs: names.iter().map(|x| x.1).collect(), done: false };
    let mut module = wasm_encoder::Module::new();
    r.parse_core_module(&mut module, wasmparser::Parser::new(0), wasm).map_err(|e| anyhow!("fpcast-prep: {e:?}"))?;
    if !r.done {
        bail!("fpcast-prep: the module has no element section");
    }
    let list: String = names.iter().map(|x| format!("{}\n", x.0)).collect();
    set_custom_section(&module.finish(), FPCAST_PREP, list.as_bytes())
}

/// After `--fpcast-emu`: export the thunk of each function [`fpcast_prep`]
/// listed as `__fpcast$<name>` and drop the passive segment, so the loader
/// fills function pointers to exports (`GOT.func`, `dlsym`) with thunks.
pub fn fpcast_post(wasm: &[u8]) -> Result<Vec<u8>> {
    use wasm_encoder::reencode::Reencode;
    let mut names: Vec<String> = Vec::new();
    let mut segs: Vec<(bool, Vec<u32>)> = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        match payload? {
            wasmparser::Payload::CustomSection(c) if c.name() == FPCAST_PREP => {
                names = std::str::from_utf8(c.data())?.lines().map(str::to_string).collect();
            }
            wasmparser::Payload::ElementSection(s) => {
                for el in s {
                    let el = el?;
                    let passive = matches!(el.kind, wasmparser::ElementKind::Passive);
                    let mut fs = Vec::new();
                    match el.items {
                        wasmparser::ElementItems::Functions(r) => {
                            for f in r {
                                fs.push(f?);
                            }
                        }
                        wasmparser::ElementItems::Expressions(_, r) => {
                            for e in r {
                                let mut ops = e?.get_operators_reader();
                                match ops.read()? {
                                    wasmparser::Operator::RefFunc { function_index } => fs.push(function_index),
                                    _ => fs.push(u32::MAX),
                                }
                            }
                        }
                    }
                    segs.push((passive, fs));
                }
            }
            _ => {}
        }
    }
    // the segment fpcast_prep added: the last passive one of the list's length
    let Some(k) = segs.iter().rposition(|(p, fs)| *p && fs.len() == names.len()) else {
        bail!("fpcast-post: the passive segment fpcast-prep added is gone");
    };
    let thunks = segs[k].1.clone();
    struct R {
        drop_seg: u32,
        seg: u32,
        add: Vec<(String, u32)>,
    }
    impl Reencode for R {
        type Error = std::convert::Infallible;
        fn parse_element(
            &mut self,
            elements: &mut wasm_encoder::ElementSection,
            element: wasmparser::Element<'_>,
        ) -> Result<(), wasm_encoder::reencode::Error<Self::Error>> {
            let i = self.seg;
            self.seg += 1;
            if i == self.drop_seg {
                return Ok(());
            }
            wasm_encoder::reencode::utils::parse_element(self, elements, element)
        }
        fn parse_export_section(
            &mut self,
            exports: &mut wasm_encoder::ExportSection,
            section: wasmparser::ExportSectionReader<'_>,
        ) -> Result<(), wasm_encoder::reencode::Error<Self::Error>> {
            wasm_encoder::reencode::utils::parse_export_section(self, exports, section)?;
            for (n, f) in &self.add {
                exports.export(n, wasm_encoder::ExportKind::Func, *f);
            }
            Ok(())
        }
        fn parse_custom_section(
            &mut self,
            module: &mut wasm_encoder::Module,
            section: wasmparser::CustomSectionReader<'_>,
        ) -> Result<(), wasm_encoder::reencode::Error<Self::Error>> {
            if section.name() == FPCAST_PREP {
                return Ok(());
            }
            wasm_encoder::reencode::utils::parse_custom_section(self, module, section)
        }
    }
    let add = names
        .iter()
        .zip(&thunks)
        .filter(|(_, &t)| t != u32::MAX)
        .map(|(n, &t)| (format!("{FPCAST_THUNK}{n}"), t))
        .collect();
    let mut r = R { drop_seg: k as u32, seg: 0, add };
    let mut module = wasm_encoder::Module::new();
    r.parse_core_module(&mut module, wasmparser::Parser::new(0), wasm).map_err(|e| anyhow!("fpcast-post: {e:?}"))?;
    Ok(module.finish())
}

/// The imports [`checked_calls`] adds: `gex.dl_stale(index)`, a function
/// the guard calls with an index this thread's table has not caught up
/// with, and `gex.dl_size`, a mutable i32 global: how many slots it has.
pub const DL_STALE: &str = "dl_stale";
pub const DL_SIZE: &str = "dl_size";

/// Guard every indirect call through table 0 (`call_indirect`,
/// `return_call_indirect`): an index at or past `gex.dl_size` calls the
/// host's `gex.dl_stale(index)` first, then the call goes ahead as before.
/// The host catches the caller's thread up with the function table (and
/// raises `dl_size`), so a function pointer from a `dlopen` that reached
/// this thread with no system call between (a lock-free handoff) is in the
/// table by the time it is called.
///
/// An opt-in check (a global read and a compare per indirect call); without
/// it such a call traps. Returns the module unchanged when it has no
/// indirect call.
pub fn checked_calls(wasm: &[u8]) -> Result<Vec<u8>> {
    use wasm_encoder::reencode::{Error as RError, Reencode};
    // the defined functions' parameter counts, the types' count, the
    // imported functions' count, and whether anything calls indirectly
    let (mut params, mut fsig, mut ntypes, mut nfi, mut ngi, mut any, mut imports) = (Vec::new(), Vec::new(), 0u32, 0u32, 0u32, false, false);
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        match payload? {
            wasmparser::Payload::TypeSection(r) => {
                for g in r {
                    for t in g?.into_types() {
                        ntypes += 1;
                        params.push(match &t.composite_type.inner {
                            wasmparser::CompositeInnerType::Func(f) => f.params().len() as u32,
                            _ => 0,
                        });
                    }
                }
            }
            wasmparser::Payload::ImportSection(r) => {
                imports = true;
                for i in r {
                    match i?.ty {
                        wasmparser::TypeRef::Func(_) => nfi += 1,
                        wasmparser::TypeRef::Global(_) => ngi += 1,
                        _ => {}
                    }
                }
            }
            wasmparser::Payload::TableSection(r) => {
                for t in r {
                    if t?.ty.table64 {
                        bail!("checked calls: a 64-bit table");
                    }
                }
            }
            wasmparser::Payload::FunctionSection(r) => {
                for f in r {
                    fsig.push(f?);
                }
            }
            wasmparser::Payload::CodeSectionEntry(body) => {
                let mut ops = body.get_operators_reader()?;
                while !ops.eof() {
                    if let wasmparser::Operator::CallIndirect { table_index: 0, .. } | wasmparser::Operator::ReturnCallIndirect { table_index: 0, .. } = ops.read()? {
                        any = true;
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    if !any {
        return Ok(wasm.to_vec());
    }
    struct R {
        nfi: u32,
        ngi: u32,
        ty: u32,
        params: Vec<u32>,
        fsig: Vec<u32>,
        body: usize,
    }
    impl Reencode for R {
        type Error = std::convert::Infallible;
        fn function_index(&mut self, f: u32) -> u32 {
            if f >= self.nfi { f + 1 } else { f }
        }
        fn global_index(&mut self, g: u32) -> u32 {
            if g >= self.ngi { g + 1 } else { g }
        }
        fn parse_type_section(
            &mut self,
            types: &mut wasm_encoder::TypeSection,
            section: wasmparser::TypeSectionReader<'_>,
        ) -> Result<(), RError<Self::Error>> {
            wasm_encoder::reencode::utils::parse_type_section(self, types, section)?;
            types.ty().function([wasm_encoder::ValType::I32], []);
            Ok(())
        }
        fn parse_import_section(
            &mut self,
            imports: &mut wasm_encoder::ImportSection,
            section: wasmparser::ImportSectionReader<'_>,
        ) -> Result<(), RError<Self::Error>> {
            wasm_encoder::reencode::utils::parse_import_section(self, imports, section)?;
            imports.import("gex", DL_STALE, wasm_encoder::EntityType::Function(self.ty));
            let size = wasm_encoder::GlobalType { val_type: wasm_encoder::ValType::I32, mutable: true, shared: false };
            imports.import("gex", DL_SIZE, wasm_encoder::EntityType::Global(size));
            Ok(())
        }
        fn parse_function_body(
            &mut self,
            code: &mut wasm_encoder::CodeSection,
            func: wasmparser::FunctionBody<'_>,
        ) -> Result<(), RError<Self::Error>> {
            use wasm_encoder::Instruction as I;
            let mut locals = Vec::new();
            let mut tmp = self.params[self.fsig[self.body] as usize];
            self.body += 1;
            for pair in func.get_locals_reader()? {
                let (n, t) = pair?;
                tmp += n;
                locals.push((n, self.val_type(t)?));
            }
            locals.push((1, wasm_encoder::ValType::I32));
            let mut f = wasm_encoder::Function::new(locals);
            let mut ops = func.get_operators_reader()?;
            while !ops.eof() {
                let op = ops.read()?;
                if let wasmparser::Operator::CallIndirect { table_index: 0, .. } | wasmparser::Operator::ReturnCallIndirect { table_index: 0, .. } = op {
                    f.instruction(&I::LocalTee(tmp));
                    f.instruction(&I::GlobalGet(self.ngi));
                    f.instruction(&I::I32GeU);
                    f.instruction(&I::If(wasm_encoder::BlockType::Empty));
                    f.instruction(&I::LocalGet(tmp));
                    f.instruction(&I::Call(self.nfi));
                    f.instruction(&I::End);
                    f.instruction(&I::LocalGet(tmp));
                }
                f.instruction(&self.instruction(op)?);
            }
            code.function(&f);
            Ok(())
        }
    }
    if params.is_empty() || !imports {
        bail!("checked calls: a module with no types or no imports");
    }
    let mut r = R { nfi, ngi, ty: ntypes, params, fsig, body: 0 };
    let mut module = wasm_encoder::Module::new();
    r.parse_core_module(&mut module, wasmparser::Parser::new(0), wasm).map_err(|e| anyhow!("checked calls: {e:?}"))?;
    let out = module.finish();
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&out)
        .context("checked calls: the rewritten module does not validate")?;
    Ok(out)
}

/// Indirect calls as C on a real machine makes them: a function called
/// through a pointer of a type with more trailing parameters than it takes
/// (GObject's class and instance initialisers, which `G_DEFINE_TYPE`
/// defines with one and GLib calls with two; a `GFunc` that is `g_free`)
/// gets the ones it takes, one that returns a value called as returning
/// none has it dropped, and one that returns none called as returning an
/// `int` gives 0 (a caller through an unprototyped `int ()`, as bash's
/// unwind-protect calls its `void` cleanups). wasm traps on such a call.
///
/// Every `call_indirect` through table 0 of a type with parameters goes to
/// a helper for its type: the call as it was when the table's function has
/// that type (`ref.test` of a `table.get`, the only cost on that path),
/// else the first type the function has of the call's parameters cut from
/// the end (with the call's result, then, for a call returning none, with
/// an `i32` one, and for one returning `i32`, with none), else the
/// original call (which traps as before). The calls stay
/// `call_indirect`, so asyncify instruments the helpers like any other
/// function. Needs `ref.test` (wasm GC) of the engine.
pub fn lenient_calls(wasm: &[u8]) -> Result<Vec<u8>> {
    use wasm_encoder::reencode::{Error as RError, Reencode};
    use wasmparser::{CompositeInnerType, Operator, Payload};
    let (mut sigs, mut nfi, mut ndef, mut used) = (Vec::new(), 0u32, 0u32, Vec::<u32>::new());
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        match payload? {
            Payload::TypeSection(r) => {
                for g in r {
                    let g = g?;
                    if g.is_explicit_rec_group() {
                        bail!("lenient calls: a recursion group");
                    }
                    for t in g.into_types() {
                        sigs.push(match &t.composite_type.inner {
                            CompositeInnerType::Func(f) => Some((f.params().to_vec(), f.results().to_vec())),
                            _ => None,
                        });
                    }
                }
            }
            Payload::ImportSection(r) => {
                for i in r {
                    if let wasmparser::TypeRef::Func(_) = i?.ty {
                        nfi += 1;
                    }
                }
            }
            Payload::FunctionSection(r) => ndef = r.count(),
            Payload::CodeSectionEntry(body) => {
                let mut ops = body.get_operators_reader()?;
                while !ops.eof() {
                    if let Operator::CallIndirect { type_index, table_index: 0 } | Operator::ReturnCallIndirect { type_index, table_index: 0 } = ops.read()? {
                        if sigs.get(type_index as usize).is_some_and(|s| s.as_ref().is_some_and(|(p, _)| !p.is_empty())) && !used.contains(&type_index) {
                            used.push(type_index);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if used.is_empty() {
        return Ok(wasm.to_vec());
    }
    used.sort();
    // the types to add: per used type, its helper's (its parameters, the
    // index), then the shorter ones in the order they are tried
    /// what a tried type's result needs: none, dropped, or a 0 made up
    #[derive(Clone, Copy, PartialEq)]
    enum Fit {
        Same,
        Drop,
        Zero,
    }
    struct Helper {
        ty: u32,
        own: u32,
        tries: Vec<(u32, u32, Fit)>,
    }
    let mut add: Vec<(Vec<wasmparser::ValType>, Vec<wasmparser::ValType>)> = Vec::new();
    let mut next = sigs.len() as u32;
    let mut helpers = Vec::new();
    for &t in &used {
        let (p, r) = sigs[t as usize].clone().unwrap();
        let mut hp = p.clone();
        hp.push(wasmparser::ValType::I32);
        add.push((hp, r.clone()));
        let own = next;
        next += 1;
        let mut tries = Vec::new();
        for k in (0..p.len()).rev() {
            add.push((p[..k].to_vec(), r.clone()));
            tries.push((next, k as u32, Fit::Same));
            next += 1;
        }
        if r.is_empty() {
            for k in (0..=p.len()).rev() {
                add.push((p[..k].to_vec(), vec![wasmparser::ValType::I32]));
                tries.push((next, k as u32, Fit::Drop));
                next += 1;
            }
        }
        if r == [wasmparser::ValType::I32] {
            for k in (0..=p.len()).rev() {
                add.push((p[..k].to_vec(), Vec::new()));
                tries.push((next, k as u32, Fit::Zero));
                next += 1;
            }
        }
        helpers.push(Helper { ty: t, own, tries });
    }
    struct R {
        add: Vec<(Vec<wasmparser::ValType>, Vec<wasmparser::ValType>)>,
        helpers: Vec<Helper>,
        sigs: Vec<Option<(Vec<wasmparser::ValType>, Vec<wasmparser::ValType>)>>,
        first: u32,
    }
    impl R {
        fn helper(&self, ty: u32) -> Option<u32> {
            self.helpers.iter().position(|h| h.ty == ty).map(|i| self.first + i as u32)
        }
    }
    impl Reencode for R {
        type Error = std::convert::Infallible;
        fn parse_type_section(&mut self, types: &mut wasm_encoder::TypeSection, section: wasmparser::TypeSectionReader<'_>) -> Result<(), RError<Self::Error>> {
            wasm_encoder::reencode::utils::parse_type_section(self, types, section)?;
            for (p, r) in std::mem::take(&mut self.add) {
                let p: Vec<_> = p.into_iter().map(|v| self.val_type(v)).collect::<Result<_, _>>()?;
                let r: Vec<_> = r.into_iter().map(|v| self.val_type(v)).collect::<Result<_, _>>()?;
                types.ty().function(p, r);
            }
            Ok(())
        }
        fn parse_function_section(&mut self, funcs: &mut wasm_encoder::FunctionSection, section: wasmparser::FunctionSectionReader<'_>) -> Result<(), RError<Self::Error>> {
            wasm_encoder::reencode::utils::parse_function_section(self, funcs, section)?;
            for h in &self.helpers {
                funcs.function(h.own);
            }
            Ok(())
        }
        fn parse_code_section(&mut self, code: &mut wasm_encoder::CodeSection, section: wasmparser::CodeSectionReader<'_>) -> Result<(), RError<Self::Error>> {
            wasm_encoder::reencode::utils::parse_code_section(self, code, section)?;
            use wasm_encoder::{BlockType, HeapType, Instruction as I};
            for h in &self.helpers {
                let n = self.sigs[h.ty as usize].as_ref().unwrap().0.len() as u32;
                // locals: the call's arguments, then the table index. The
                // function is read from the table for each test rather than
                // kept in a local: asyncify keeps no reference-typed local.
                let mut f = wasm_encoder::Function::new([]);
                let call = |f: &mut wasm_encoder::Function, ty: u32, k: u32, fit: Fit| {
                    f.instruction(&I::LocalGet(n)).instruction(&I::TableGet(0));
                    f.instruction(&I::RefTestNonNull(HeapType::Concrete(ty))).instruction(&I::If(BlockType::Empty));
                    for a in 0..k {
                        f.instruction(&I::LocalGet(a));
                    }
                    f.instruction(&I::LocalGet(n)).instruction(&I::CallIndirect { type_index: ty, table_index: 0 });
                    match fit {
                        Fit::Same => {}
                        Fit::Drop => {
                            f.instruction(&I::Drop);
                        }
                        Fit::Zero => {
                            f.instruction(&I::I32Const(0));
                        }
                    }
                    f.instruction(&I::Return).instruction(&I::End);
                };
                call(&mut f, h.ty, n, Fit::Same);
                for &(ty, k, fit) in &h.tries {
                    call(&mut f, ty, k, fit);
                }
                for a in 0..=n {
                    f.instruction(&I::LocalGet(a));
                }
                f.instruction(&I::CallIndirect { type_index: h.ty, table_index: 0 }).instruction(&I::End);
                code.function(&f);
            }
            Ok(())
        }
        fn instruction<'a>(&mut self, op: wasmparser::Operator<'a>) -> Result<wasm_encoder::Instruction<'a>, RError<Self::Error>> {
            use wasm_encoder::Instruction as I;
            match op {
                wasmparser::Operator::CallIndirect { type_index, table_index: 0 } => {
                    if let Some(h) = self.helper(type_index) {
                        return Ok(I::Call(h));
                    }
                }
                wasmparser::Operator::ReturnCallIndirect { type_index, table_index: 0 } => {
                    if let Some(h) = self.helper(type_index) {
                        return Ok(I::ReturnCall(h));
                    }
                }
                _ => {}
            }
            wasm_encoder::reencode::utils::instruction(self, op)
        }
    }
    let mut r = R { add, helpers, sigs, first: nfi + ndef };
    let mut module = wasm_encoder::Module::new();
    r.parse_core_module(&mut module, wasmparser::Parser::new(0), wasm).map_err(|e| anyhow!("lenient calls: {e:?}"))?;
    let out = module.finish();
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&out)
        .context("lenient calls: the rewritten module does not validate")?;
    Ok(out)
}

/// A data relocation function's body past this many bytes is split: V8 and
/// wasmparser refuse a function body past 7654321 bytes, and one large body
/// is compiled by one thread.
const SPLIT_ABOVE: usize = 1 << 20;
/// The size of each piece it is split into.
const SPLIT_PIECE: usize = 256 << 10;

/// The relocation functions wasm-ld writes (`__wasm_apply_data_relocs`,
/// `__wasm_apply_global_relocs`), when one is past [`SPLIT_ABOVE`] bytes,
/// as calls to pieces appended as new functions of the same type. A large
/// program's data relocations grow past what an engine compiles (Chromium's
/// is 8 MB). Their bodies are independent stores of constants and globals'
/// sums; a body with anything else is left whole. `None`: nothing to split.
pub fn split_relocs(wasm: &[u8]) -> Result<Option<Vec<u8>>> {
    use wasmparser::{Operator as O, Payload as P};
    let names = ["__wasm_apply_data_relocs", "__wasm_apply_global_relocs"];
    let mut nimp = 0u32;
    let mut functypes: Vec<u32> = Vec::new();
    let mut nparams: Vec<(usize, usize)> = Vec::new();
    let mut targets: HashMap<u32, String> = HashMap::default();
    // per function to split: its pieces' byte ranges in `wasm`, and its locals' bytes
    let mut pieces: HashMap<u32, (Vec<std::ops::Range<usize>>, std::ops::Range<usize>)> = HashMap::default();
    let mut fi = 0u32;
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        match payload? {
            P::TypeSection(r) => {
                for t in r.into_iter_err_on_gc_types() {
                    let t = t?;
                    nparams.push((t.params().len(), t.results().len()));
                }
            }
            P::ImportSection(r) => {
                for i in r {
                    if matches!(i?.ty, wasmparser::TypeRef::Func(_)) {
                        nimp += 1;
                    }
                }
            }
            P::FunctionSection(r) => {
                for t in r {
                    functypes.push(t?);
                }
            }
            P::ExportSection(r) => {
                for e in r {
                    let e = e?;
                    if e.kind == wasmparser::ExternalKind::Func && names.contains(&e.name) {
                        targets.insert(e.index, e.name.to_string());
                    }
                }
            }
            P::CodeSectionEntry(body) => {
                let idx = nimp + fi;
                fi += 1;
                let range = body.range();
                if !targets.contains_key(&idx) || range.len() <= SPLIT_ABOVE {
                    continue;
                }
                let ty = functypes[(idx - nimp) as usize] as usize;
                if nparams.get(ty) != Some(&(0, 0)) {
                    continue;
                }
                let locals = body.get_locals_reader()?;
                let code_at = locals.original_position();
                let mut ops = body.get_operators_reader()?;
                let mut depth = 0i32;
                let mut cuts = vec![ops.original_position()];
                let mut ok = true;
                while !ops.eof() {
                    let at = ops.original_position();
                    if depth == 0 && at - cuts[cuts.len() - 1] >= SPLIT_PIECE {
                        cuts.push(at);
                    }
                    depth += match ops.read()? {
                        O::I32Const { .. } | O::I64Const { .. } | O::GlobalGet { .. } => 1,
                        O::I32Add | O::I64Add => -1,
                        O::I32Store { .. } | O::I64Store { .. } => -2,
                        O::End if ops.eof() => {
                            cuts.push(at);
                            0
                        }
                        _ => {
                            ok = false;
                            break;
                        }
                    };
                }
                if ok && depth == 0 {
                    let rs = cuts.windows(2).map(|w| w[0]..w[1]).collect();
                    pieces.insert(idx, (rs, range.start..code_at));
                }
            }
            _ => {}
        }
    }
    if pieces.is_empty() {
        return Ok(None);
    }
    // the pieces, numbered after every function, in function order
    let mut order: Vec<u32> = pieces.keys().copied().collect();
    order.sort();
    let mut next = nimp + functypes.len() as u32;
    let mut first: HashMap<u32, u32> = HashMap::default();
    for i in &order {
        first.insert(*i, next);
        next += pieces[i].0.len() as u32;
    }
    let added = next - nimp - functypes.len() as u32;
    let section = |out: &mut Vec<u8>, id: u8, body: &[u8]| {
        out.push(id);
        leb(out, body.len() as u32);
        out.extend_from_slice(body);
    };
    let mut out = wasm[..8].to_vec();
    let mut code = Vec::new();
    let mut fi = 0u32;
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        let payload = payload?;
        match &payload {
            P::FunctionSection(_) => {
                let mut b = Vec::new();
                leb(&mut b, functypes.len() as u32 + added);
                for t in &functypes {
                    leb(&mut b, *t);
                }
                for i in &order {
                    for _ in &pieces[i].0 {
                        leb(&mut b, functypes[(i - nimp) as usize]);
                    }
                }
                section(&mut out, 3, &b);
            }
            P::CodeSectionStart { .. } => {}
            P::CodeSectionEntry(body) => {
                let idx = nimp + fi;
                fi += 1;
                let mut f = Vec::new();
                if let Some((rs, _)) = pieces.get(&idx) {
                    f.push(0);
                    for k in 0..rs.len() as u32 {
                        f.push(0x10);
                        leb(&mut f, first[&idx] + k);
                    }
                    f.push(0x0b);
                } else {
                    f.extend_from_slice(&wasm[body.range()]);
                }
                leb(&mut code, f.len() as u32);
                code.extend_from_slice(&f);
                if fi as usize == functypes.len() {
                    let mut b = Vec::new();
                    leb(&mut b, functypes.len() as u32 + added);
                    b.extend_from_slice(&code);
                    for i in &order {
                        let (rs, locals) = &pieces[i];
                        for r in rs {
                            let mut f = wasm[locals.clone()].to_vec();
                            f.extend_from_slice(&wasm[r.clone()]);
                            f.push(0x0b);
                            leb(&mut b, f.len() as u32);
                            b.extend_from_slice(&f);
                        }
                    }
                    section(&mut out, 10, &b);
                }
            }
            P::CustomSection(c) if c.name() == "name" => {
                let mut b = Vec::new();
                leb(&mut b, 4);
                b.extend_from_slice(b"name");
                let mut r = wasmparser::BinaryReader::new(c.data(), 0);
                while !r.eof() {
                    let id = r.read_u8()?;
                    let len = r.read_var_u32()? as usize;
                    let sub = r.read_bytes(len)?;
                    if id != 1 {
                        b.push(id);
                        leb(&mut b, len as u32);
                        b.extend_from_slice(sub);
                        continue;
                    }
                    let mut sr = wasmparser::BinaryReader::new(sub, 0);
                    let n = sr.read_var_u32()?;
                    let mut s = Vec::new();
                    leb(&mut s, n + added);
                    s.extend_from_slice(&sub[sr.current_position()..]);
                    for i in &order {
                        for k in 0..pieces[i].0.len() as u32 {
                            let nm = format!("{}.{k}", targets[i]);
                            leb(&mut s, first[i] + k);
                            leb(&mut s, nm.len() as u32);
                            s.extend_from_slice(nm.as_bytes());
                        }
                    }
                    b.push(1);
                    leb(&mut b, s.len() as u32);
                    b.extend_from_slice(&s);
                }
                section(&mut out, 0, &b);
            }
            _ => {
                if let Some((id, range)) = payload.as_section() {
                    section(&mut out, id, &wasm[range]);
                }
            }
        }
    }
    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&out)
        .context("split relocations: the rewritten module does not validate")?;
    Ok(Some(out))
}

/// Add (or replace) a custom section at the end of a module.
pub fn set_custom_section(wasm: &[u8], name: &str, data: &[u8]) -> Result<Vec<u8>> {
    let mut out = wasm[..8].to_vec();
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        let payload = payload?;
        if let wasmparser::Payload::CustomSection(c) = &payload {
            if c.name() == name {
                continue;
            }
        }
        if let Some((id, range)) = payload.as_section() {
            // the section's header (id, size) precedes its contents
            let mut hdr = vec![id];
            leb(&mut hdr, range.len() as u32);
            out.extend_from_slice(&hdr);
            out.extend_from_slice(&wasm[range]);
        }
    }
    let mut body = Vec::new();
    leb(&mut body, name.len() as u32);
    body.extend_from_slice(name.as_bytes());
    body.extend_from_slice(data);
    out.push(0);
    leb(&mut out, body.len() as u32);
    out.extend_from_slice(&body);
    Ok(out)
}

fn leb(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// One module of a process's link map.
#[derive(Clone, Debug)]
pub struct Loaded {
    /// The name it was asked for by (a `NEEDED` soname, or the program's path).
    pub name: String,
    /// Where it was found.
    pub path: String,
    pub info: Info,
    pub memory_base: u32,
    pub table_base: u32,
}

/// What one import of one module is bound to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bind {
    /// Left to the shim: `env.memory`, the table, `__memory_base`,
    /// `__table_base`, `__stack_pointer`, `gex.*` and the like.
    Shim,
    /// A `GOT.mem` global's value (an address).
    GotMem(u32),
    /// A `GOT.func` global's value (a table index).
    GotFunc(u32),
    /// An export of a module instantiated before this one (an index into
    /// the link map).
    Export { module: usize, name: String },
    /// A function of a module instantiated after this one (a cycle, or a
    /// symbol of the program a library uses): called through its table slot.
    Late { slot: u32 },
    /// A weak undefined symbol, or a strong function nobody defines
    /// (an error when called, as lazy binding makes it): a function that
    /// traps with this message, or 0 for a global.
    Missing(String),
}

/// A process's modules: the program and, breadth first, its libraries'
/// (the global scope, in lookup order).
#[derive(Clone, Debug, Default)]
pub struct Link {
    pub mods: Vec<Loaded>,
    /// [`Link::lookup`]'s index, kept up with `mods` as they are added.
    pub scope: Scope,
}

/// The first module in scope order defining each function and each data
/// symbol, of the first `upto` modules: a lookup is one hash, not one per
/// module until a definition (a library binding tens of thousands of
/// symbols against a hundred others). Modules are only ever added to a
/// link map, after the ones there.
#[derive(Default)]
pub struct Scope(std::sync::Mutex<ScopeIndex>);

#[derive(Default)]
struct ScopeIndex {
    upto: usize,
    /// The defining module of a function (`.0`) and a global (`.1`), by
    /// the name's hash ([`name_hash`]): no copy of each of a hundred
    /// thousand names per process. A lookup checks the module it finds
    /// does define the name, and two names of one hash fall back to a scan.
    first: std::collections::HashMap<u64, (u32, u32), std::hash::BuildHasherDefault<IdHasher>>,
}

/// A name's 64-bit hash, the scope index's key.
fn name_hash(name: &str) -> u64 {
    use std::hash::Hasher;
    let mut h = FxHasher::default();
    h.write(name.as_bytes());
    h.finish()
}

/// The scope index's hasher: its keys are hashes already.
#[derive(Default)]
struct IdHasher(u64);

impl std::hash::Hasher for IdHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 << 8) ^ b as u64;
        }
    }
    fn write_u64(&mut self, v: u64) {
        self.0 = v;
    }
}

impl Clone for Scope {
    /// Empty: the clone's modules may change apart from these.
    fn clone(&self) -> Scope {
        Scope::default()
    }
}

impl std::fmt::Debug for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Scope")
    }
}

const NONE: u32 = u32::MAX;

/// Imports the shim provides.
pub fn shim_import(m: &str, name: &str) -> bool {
    match m {
        "env" => matches!(
            name,
            "memory"
                | "__indirect_function_table"
                | "__stack_pointer"
                | "__memory_base"
                | "__table_base"
                // a lowered wasm64 module's i32 copy of it (ftab.rs)
                | "__table_base32"
                | "__asyncify_state"
                | "__asyncify_data"
        ),
        "GOT.mem" | "GOT.func" => false,
        _ => true,
    }
}

impl Link {
    /// Back to its first `n` modules (a dlopen that failed part way): the
    /// scope index forgets what it had of the rest.
    pub fn truncate(&mut self, n: usize) {
        if n >= self.mods.len() {
            return;
        }
        self.mods.truncate(n);
        let mut x = self.scope.0.lock().unwrap_or_else(|e| e.into_inner());
        if x.upto > n {
            *x = ScopeIndex::default();
        }
    }

    /// Whether the program is a static one (it names no libraries): what a
    /// library it `dlopen`s imports, its libc's included, is the program's
    /// own exports (`-Wl,--export`), and no `libc.so` is loaded beside it.
    pub fn static_program(&self) -> bool {
        self.mods.first().is_some_and(|m| m.info.needed.is_empty())
    }

    /// The run path `dlopen` searches a bare soname in before
    /// `LD_LIBRARY_PATH`: a static program's is its own directory
    /// (`$ORIGIN`), where the side modules it may load lie beside it (it has
    /// no `dylink.0` to name one); a dynamic one's is none.
    pub fn dlopen_runpath(&self) -> Vec<String> {
        if self.static_program() { vec!["$ORIGIN".to_string()] } else { Vec::new() }
    }

    /// The program's libraries, breadth first from its `NEEDED`: for each,
    /// `find(soname, runpath)` gives its path and bytes (searching the run
    /// path, `LD_LIBRARY_PATH` and [`DEFAULT_DIRS`] is the caller's, through
    /// [`search`]). The run path is the needing object's, with `$ORIGIN`
    /// already its directory (as `ld.so` has it), not the program's.
    /// Libraries already in the link map (by soname) are not loaded again.
    pub fn needed_of(
        &self,
        first: usize,
        find: impl FnMut(&str, &[String]) -> Result<(String, Vec<u8>)>,
    ) -> Result<Vec<(String, String, Vec<u8>, Info)>> {
        self.needed_of_shed(first, find, |_, b| b)
    }

    /// [`Link::needed_of`], each library's bytes handed to `shed` once they
    /// are parsed, which gives back what to keep: a loader that can read a
    /// file again keeps none, so a program's libraries are never all in
    /// memory at once.
    pub fn needed_of_shed(
        &self,
        first: usize,
        mut find: impl FnMut(&str, &[String]) -> Result<(String, Vec<u8>)>,
        mut shed: impl FnMut(&str, Vec<u8>) -> Vec<u8>,
    ) -> Result<Vec<(String, String, Vec<u8>, Info)>> {
        let mut have: HashSet<String> = self.mods.iter().map(|m| m.name.clone()).collect();
        // a static program is its own libc
        if self.static_program() {
            have.insert(LIBC_SONAME.to_string());
        }
        let mut out = Vec::new();
        let mut q: VecDeque<(Vec<String>, Vec<String>)> = self.mods[first..]
            .iter()
            .map(|m| (m.info.needed.clone(), origin_expanded(&m.info.runpath, &m.path)))
            .collect();
        while let Some((needed, rp)) = q.pop_front() {
            for n in needed {
                if !have.insert(n.clone()) {
                    continue;
                }
                let (path, bytes) = find(&n, &rp)?;
                let info = Info::parse(&bytes)
                    .with_context(|| format!("{path}: not a wasm shared library"))?;
                if !info.dylink {
                    bail!("{path}: not a wasm shared library (no dylink.0)");
                }
                q.push_back((info.needed.clone(), origin_expanded(&info.runpath, &path)));
                let bytes = shed(&path, bytes);
                out.push((n, path, bytes, info));
            }
        }
        Ok(out)
    }

    /// The order to instantiate `mods[from..]` in: each after what it
    /// needs (depth first, post-order), cycles broken where they close.
    pub fn init_order(&self, from: usize) -> Vec<usize> {
        let by_name: HashMap<&str, usize> = self
            .mods
            .iter()
            .enumerate()
            .map(|(i, m)| (m.name.as_str(), i))
            .collect();
        let mut state = vec![0u8; self.mods.len()];
        for s in state.iter_mut().take(from) {
            *s = 2;
        }
        let mut out = Vec::new();
        fn visit(
            i: usize,
            link: &Link,
            by_name: &HashMap<&str, usize>,
            state: &mut [u8],
            out: &mut Vec<usize>,
        ) {
            if state[i] != 0 {
                return;
            }
            state[i] = 1;
            for n in &link.mods[i].info.needed {
                if let Some(&j) = by_name.get(n.as_str()) {
                    visit(j, link, by_name, state, out);
                }
            }
            state[i] = 2;
            out.push(i);
        }
        for i in from..self.mods.len() {
            visit(i, self, &by_name, &mut state, &mut out);
        }
        out
    }

    /// The first module in scope order defining `name` as `kind`.
    pub fn lookup(&self, name: &str, kind: Kind) -> Option<usize> {
        if matches!(kind, Kind::Func | Kind::Global) {
            let mut x = self.scope.0.lock().unwrap_or_else(|e| e.into_inner());
            if x.upto > self.mods.len() {
                *x = ScopeIndex::default();
            }
            let more: usize = self.mods[x.upto.min(self.mods.len())..].iter().map(|m| m.info.exports.len()).sum();
            x.first.reserve(more);
            for i in x.upto..self.mods.len() {
                let m = &self.mods[i].info;
                for (n, e) in &m.exports {
                    let slot = match e.kind {
                        Kind::Func if e.index >= m.func_imports => 0,
                        Kind::Global if e.index >= m.global_imports => 1,
                        _ => continue,
                    };
                    let f = x.first.entry(name_hash(n)).or_insert((NONE, NONE));
                    let at = if slot == 0 { &mut f.0 } else { &mut f.1 };
                    if *at == NONE {
                        *at = i as u32;
                    }
                }
            }
            x.upto = self.mods.len();
            let defines = |i: usize| {
                let m = &self.mods[i].info;
                m.exports.get(name).is_some_and(|e| {
                    e.kind == kind
                        && match kind {
                            Kind::Func => e.index >= m.func_imports,
                            _ => e.index >= m.global_imports,
                        }
                })
            };
            let at = x.first.get(&name_hash(name)).map_or(NONE, |f| if kind == Kind::Func { f.0 } else { f.1 });
            drop(x);
            if at == NONE {
                return None;
            }
            // The first module of this hash defines the name, or another
            // name of the same hash came first: the earliest definer then
            // is found by a scan (no module before `at` defines a name of
            // this hash, so none before it defines this one).
            if defines(at as usize) {
                return Some(at as usize);
            }
            return (at as usize + 1..self.mods.len()).find(|&i| defines(i));
        }
        self.mods.iter().position(|m| {
            m.info.exports.get(name).is_some_and(|e| {
                e.kind == kind
                    && match kind {
                        Kind::Func => e.index >= m.info.func_imports,
                        Kind::Global => e.index >= m.info.global_imports,
                        _ => true,
                    }
            })
        })
    }

    /// The table index of `name`'s function, if a module defines it.
    pub fn func_slot(&self, name: &str) -> Option<u32> {
        let d = self.lookup(name, Kind::Func)?;
        let m = &self.mods[d];
        let (_, at) = m.info.slots();
        at.get(&m.info.exports[name].index)
            .map(|&s| m.table_base + s)
    }

    /// A data symbol's address, if a module defines it.
    pub fn data_addr(&self, name: &str) -> Option<u32> {
        let d = self.lookup(name, Kind::Global)?;
        let m = &self.mods[d];
        m.info
            .data_offset(name)
            .map(|o| m.memory_base.wrapping_add(o))
    }

    /// What each import of module `i` is bound to, given which modules are
    /// instantiated before it (`done`).
    pub fn bind<S: std::hash::BuildHasher>(&self, i: usize, done: &std::collections::HashSet<usize, S>) -> Result<Vec<Bind>> {
        let m = &self.mods[i];
        let mut out = Vec::with_capacity(m.info.imports.len());
        for imp in &m.info.imports {
            let b = match imp.module.as_str() {
                "GOT.mem" => match self.data_addr(&imp.name) {
                    Some(a) => Bind::GotMem(a),
                    None if imp.weak => Bind::GotMem(0),
                    // a function's address asked for as data (wasm-ld does
                    // this for an undefined symbol it cannot type)
                    None => match self.func_slot(&imp.name) {
                        Some(s) => Bind::GotMem(s),
                        None => bail!(
                            "{}: symbol lookup error: undefined symbol: {}",
                            m.path,
                            imp.name
                        ),
                    },
                },
                "GOT.func" => match self.func_slot(&imp.name) {
                    Some(s) => Bind::GotFunc(s),
                    None if imp.weak => Bind::GotFunc(0),
                    None => Bind::Missing(format!("{}: undefined symbol: {}", m.path, imp.name)),
                },
                "env" if !shim_import("env", &imp.name) => {
                    let kind = imp.kind;
                    match self.lookup(&imp.name, kind) {
                        Some(d) if done.contains(&d) => Bind::Export { module: d, name: imp.name.clone() },
                        Some(_) if kind == Kind::Func => match self.func_slot(&imp.name) {
                            Some(slot) => Bind::Late { slot },
                            None => Bind::Missing(format!("{}: {} has no table slot", m.path, imp.name)),
                        },
                        Some(d) => bail!(
                            "{}: {} is defined by {}, which loads after it (a cycle of non-function symbols)",
                            m.path,
                            imp.name,
                            self.mods[d].path
                        ),
                        None if kind == Kind::Func => Bind::Missing(format!("{}: undefined symbol: {}", m.path, imp.name)),
                        None if imp.weak => Bind::Missing(String::new()),
                        None => bail!("{}: symbol lookup error: undefined symbol: {}", m.path, imp.name),
                    }
                }
                _ => Bind::Shim,
            };
            out.push(b);
        }
        Ok(out)
    }
}

/// The soname of gexcc's shared libc, which every library names.
pub const LIBC_SONAME: &str = "libc.so";

/// `dlopen` handles gex gives (libgex's dl.c): above everything a 32-bit
/// process maps data at, one per link-map entry.
pub const HANDLE_BASE: u32 = 0xfff0_0000;

impl Info {
    /// An exported global's constant value (`__tls_size`), 0 for none.
    pub fn value(&self, name: &str) -> u32 {
        self.exports
            .get(name)
            .filter(|e| e.kind == Kind::Global && e.index >= self.global_imports)
            .and_then(|e| self.global_init.get((e.index - self.global_imports) as usize).copied().flatten())
            .unwrap_or(0)
    }
}

impl Loaded {
    /// The table index of this module's own exported function `name`, 0 for none.
    pub fn own_func(&self, name: &str) -> u32 {
        let (_, slots) = self.info.slots();
        self.info
            .exports
            .get(name)
            .filter(|e| e.kind == Kind::Func)
            .and_then(|e| slots.get(&e.index))
            .map_or(0, |&s| self.table_base + s)
    }
}

impl Link {
    /// The link-map entry `dlopen(path)` names, if loaded: by path, by the
    /// name it was loaded as, or, for a bare name, by soname.
    pub fn loaded(&self, path: &str) -> Option<usize> {
        let base = path.rsplit('/').next().unwrap_or(path);
        self.mods
            .iter()
            .position(|m| m.path == path || m.name == path || (!path.contains('/') && m.name == base))
    }

    /// `dlsym(handle, name)`: a function's table index or a datum's
    /// address; 0 when not found. Handle 0: the global scope; otherwise
    /// the library and what it needs, breadth first.
    pub fn dlsym(&self, handle: u32, name: &str) -> u32 {
        let scope: Vec<usize> = if handle == 0 {
            (0..self.mods.len()).collect()
        } else {
            let first = (handle.wrapping_sub(HANDLE_BASE) / 16) as usize;
            if first >= self.mods.len() {
                return 0;
            }
            let mut seen = vec![first];
            let mut k = 0;
            while k < seen.len() {
                for n in &self.mods[seen[k]].info.needed {
                    if let Some(j) = self.mods.iter().position(|m| &m.name == n) {
                        if !seen.contains(&j) {
                            seen.push(j);
                        }
                    }
                }
                k += 1;
            }
            seen
        };
        for i in scope {
            let m = &self.mods[i];
            let Some(e) = m.info.exports.get(name) else { continue };
            match e.kind {
                Kind::Func if e.index >= m.info.func_imports => {
                    let (_, at) = m.info.slots();
                    if let Some(&s) = at.get(&e.index) {
                        return m.table_base + s;
                    }
                }
                Kind::Global => {
                    if let Some(o) = m.info.data_offset(name) {
                        return m.memory_base.wrapping_add(o);
                    }
                }
                _ => {}
            }
        }
        0
    }

    /// Where libc's `__gex_dsos` (musl's gex_dso.h) is in the process's
    /// memory, libc being module `libc`.
    pub fn dsos_at(&self, libc: usize) -> Result<usize> {
        let lc = &self.mods[libc];
        let Some(off) = lc.info.data_offset("__gex_dsos") else {
            bail!("{}: no __gex_dsos (a libc.so older than gex's loader)", lc.path);
        };
        Ok((lc.memory_base + off) as usize)
    }

    /// How many bytes `__gex_dsos` takes, from its first 12 (`head`).
    pub fn dsos_len(head: &[u8]) -> Option<usize> {
        Some(12 + u32::from_le_bytes(head.get(8..12)?.try_into().ok()?) as usize * 24)
    }

    /// Fill libc's `__gex_dsos` (musl's gex_dso.h), `dsos` being its bytes
    /// in the process's memory (at [`Self::dsos_at`], [`Self::dsos_len`]
    /// long): every module but libc (index `libc`), in init order `order`,
    /// with its TLS image and functions (table indices), libc's own
    /// constructors at the head; the program's constructors are crt1's to
    /// run.
    pub fn write_registry(&self, dsos: &mut [u8], order: &[usize], libc: usize) -> Result<()> {
        let lc = &self.mods[libc];
        let rd = |b: &[u8], a: usize| u32::from_le_bytes(b[a..a + 4].try_into().unwrap());
        if dsos.len() < 12 {
            bail!("{}: __gex_dsos outside memory", lc.path);
        }
        let (version, max) = (rd(dsos, 0), rd(dsos, 8));
        if version != 1 {
            bail!("{}: __gex_dsos version {version} (want 1)", lc.path);
        }
        if dsos.len() < 12 + max as usize * 24 {
            bail!("{}: __gex_dsos outside memory", lc.path);
        }
        let mut entries: Vec<[u32; 6]> = Vec::new();
        // libc's own constructors first (it is initialised before anything
        // it serves)
        let lctors = lc.own_func("__wasm_call_ctors");
        if lctors != 0 {
            entries.push([0, 1, 0, 0, lctors, 0]);
        }
        for &i in order {
            if i == libc {
                continue;
            }
            let m = &self.mods[i];
            let tls_size = m.info.value("__tls_size");
            let w = [
                tls_size,
                m.info.value("__tls_align").max(1),
                if tls_size != 0 { m.own_func("__wasm_init_tls") } else { 0 },
                if tls_size != 0 { m.own_func("__gex_set_tls_base") } else { 0 },
                if i == 0 { 0 } else { m.own_func("__wasm_call_ctors") },
                0,
            ];
            if tls_size != 0 && (w[2] == 0 || w[3] == 0) {
                bail!("{}: has thread-local data but does not export __wasm_init_tls and __gex_set_tls_base", m.path);
            }
            entries.push(w);
        }
        if entries.len() > max as usize {
            bail!("more than {max} shared libraries");
        }
        for (k, w) in entries.iter().enumerate() {
            for (j, x) in w.iter().enumerate() {
                let a = 12 + k * 24 + j * 4;
                dsos[a..a + 4].copy_from_slice(&x.to_le_bytes());
            }
        }
        dsos[4..8].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        Ok(())
    }
}

/// `runpath` with `$ORIGIN` as the directory of the object at `path`.
fn origin_expanded(runpath: &[String], path: &str) -> Vec<String> {
    let dir = path.rsplit_once('/').map(|(d, _)| if d.is_empty() { "/" } else { d }).unwrap_or(".");
    runpath.iter().map(|r| r.replace("${ORIGIN}", dir).replace("$ORIGIN", dir)).collect()
}

/// Search for library `name` (a soname, or a path when it has a `/`): the
/// run path (`$ORIGIN` left in it is `origin`, the program's directory),
/// `ld_library_path`, then [`DEFAULT_DIRS`]. `read` gives a file's bytes.
pub fn search(
    name: &str,
    runpath: &[String],
    origin: &str,
    ld_library_path: &[String],
    mut read: impl FnMut(&str) -> Option<Vec<u8>>,
) -> Result<(String, Vec<u8>)> {
    if name.contains('/') {
        return read(name)
            .map(|b| (name.to_string(), b))
            .ok_or_else(|| anyhow!("{name}: cannot open shared object file"));
    }
    let dirs = runpath
        .iter()
        .flat_map(|r| r.split(':'))
        .map(|d| d.replace("$ORIGIN", origin).replace("${ORIGIN}", origin))
        .chain(ld_library_path.iter().cloned())
        .chain(DEFAULT_DIRS.iter().map(|s| s.to_string()));
    for d in dirs {
        if d.is_empty() {
            continue;
        }
        let p = format!("{}/{name}", d.trim_end_matches('/'));
        if let Some(b) = read(&p) {
            if b.starts_with(b"\0asm") {
                return Ok((p, b));
            }
        }
    }
    bail!("error while loading shared libraries: {name}: cannot open shared object file: No such file or directory")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(name: &str, func_imports: u32, exports: &[(&str, Kind, u32)]) -> Loaded {
        let info = Info {
            func_imports,
            global_imports: 1,
            exports: exports.iter().map(|&(n, kind, index)| (n.to_string(), Export { kind, index })).collect(),
            ..Default::default()
        };
        Loaded { name: name.into(), path: name.into(), info, memory_base: 0, table_base: 0 }
    }

    #[test]
    fn lookup_takes_the_first_definition_in_scope_order() {
        let mut l = Link::default();
        // an export of an import is no definition
        l.mods.push(module("prog", 2, &[("f", Kind::Func, 1), ("g", Kind::Global, 0)]));
        l.mods.push(module("liba", 0, &[("f", Kind::Func, 3), ("t", Kind::Table, 0)]));
        l.mods.push(module("libb", 0, &[("f", Kind::Func, 4), ("g", Kind::Global, 2), ("h", Kind::Func, 0)]));
        assert_eq!(l.lookup("f", Kind::Func), Some(1));
        assert_eq!(l.lookup("g", Kind::Global), Some(2));
        assert_eq!(l.lookup("g", Kind::Func), None);
        assert_eq!(l.lookup("t", Kind::Table), Some(1));
        assert_eq!(l.lookup("nope", Kind::Func), None);
        // a module added later (dlopen) comes after the ones there
        l.mods.push(module("libc", 0, &[("h", Kind::Func, 1), ("k", Kind::Func, 2)]));
        assert_eq!(l.lookup("h", Kind::Func), Some(2));
        assert_eq!(l.lookup("k", Kind::Func), Some(3));
        // a clone keeps its own index
        let mut c = l.clone();
        c.mods.truncate(3);
        assert_eq!(c.lookup("k", Kind::Func), None);
        assert_eq!(l.lookup("k", Kind::Func), Some(3));
    }

    /// `wasm` with only the sections [`Info::reads_section`] names, as the
    /// browser host reads a library it has compiled (`read_wasm_parts`).
    fn info_parts(wasm: &[u8]) -> Vec<u8> {
        let mut out = wasm[..8].to_vec();
        for p in wasmparser::Parser::new(0).parse_all(wasm) {
            let p = p.unwrap();
            let (id, body) = match &p {
                wasmparser::Payload::CodeSectionStart { range, .. } => (10, range.clone()),
                wasmparser::Payload::CustomSection(c) => (0, c.range()),
                _ => match p.as_section() {
                    Some((id, r)) => (id, r),
                    None => continue,
                },
            };
            // the section's id and length come before its body
            let mut at = body.start - 1;
            while wasm[at - 1] & 0x80 != 0 {
                at -= 1;
            }
            let start = at - 1;
            assert_eq!(wasm[start], id);
            let name: &[u8] = if id == 0 { let n = wasm[body.start] as usize; &wasm[body.start + 1..body.start + 1 + n] } else { &[] };
            if Info::reads_section(id, name) {
                out.extend_from_slice(&wasm[start..body.end]);
            }
        }
        out
    }

    /// A library's Info from its sections that `Info::parse` reads is the
    /// one from the whole file. `GEX_DYLINK_PARTS_DIR` names a directory
    /// of libraries to check too (a sysroot's lib).
    #[test]
    fn info_from_its_sections() {
        let mut libs: Vec<(String, Vec<u8>)> = Vec::new();
        if let Ok(d) = std::env::var("GEX_DYLINK_PARTS_DIR") {
            for e in std::fs::read_dir(&d).unwrap() {
                let e = e.unwrap();
                if e.file_type().unwrap().is_file() {
                    let b = std::fs::read(e.path()).unwrap();
                    if b.starts_with(b"\0asm") {
                        libs.push((e.path().display().to_string(), b));
                    }
                }
            }
            assert!(!libs.is_empty(), "{d}: no wasm modules");
        }
        libs.push(("made here".into(), made_library()));
        for (name, b) in &libs {
            let full = Info::parse(b).unwrap();
            let cut = Info::parse(&info_parts(b)).unwrap();
            assert_eq!(format!("{full:?}"), format!("{cut:?}"), "{name}");
        }
    }

    /// A side module with code, data, a table, a TLS export and a dylink.0.
    fn made_library() -> Vec<u8> {
        use wasm_encoder::*;
        let mut m = Module::new();
        let mut d = Vec::new();
        // WASM_DYLINK_MEM_INFO: mem size 16, align 2, table 1, align 0
        d.extend_from_slice(&[1, 4, 16, 2, 1, 0]);
        // WASM_DYLINK_NEEDED: libc.so
        d.extend_from_slice(&[2, 9, 1, 7]);
        d.extend_from_slice(b"libc.so");
        m.section(&CustomSection { name: "dylink.0".into(), data: d.into() });
        let mut t = TypeSection::new();
        t.ty().function([ValType::I32], [ValType::I32]);
        m.section(&t);
        let mut i = ImportSection::new();
        i.import("env", "memory", MemoryType { minimum: 1, maximum: None, memory64: false, shared: false, page_size_log2: None });
        i.import("env", "__indirect_function_table", TableType { element_type: RefType::FUNCREF, minimum: 1, maximum: None, table64: false, shared: false });
        i.import("env", "__memory_base", GlobalType { val_type: ValType::I32, mutable: false, shared: false });
        i.import("env", "__table_base", GlobalType { val_type: ValType::I32, mutable: false, shared: false });
        i.import("env", "puts", EntityType::Function(0));
        m.section(&i);
        let mut f = FunctionSection::new();
        f.function(0);
        f.function(0);
        m.section(&f);
        let mut g = GlobalSection::new();
        g.global(GlobalType { val_type: ValType::I32, mutable: false, shared: false }, &ConstExpr::i32_const(8));
        m.section(&g);
        let mut e = ExportSection::new();
        e.export("f", ExportKind::Func, 1);
        e.export("g", ExportKind::Func, 2);
        e.export("puts_again", ExportKind::Func, 0);
        e.export("data", ExportKind::Global, 2);
        m.section(&e);
        let mut el = ElementSection::new();
        el.active(Some(0), &ConstExpr::global_get(1), Elements::Functions(std::borrow::Cow::Borrowed(&[2])));
        m.section(&el);
        let mut c = CodeSection::new();
        for _ in 0..2 {
            let mut b = Function::new([]);
            b.instruction(&Instruction::LocalGet(0));
            b.instruction(&Instruction::End);
            c.function(&b);
        }
        m.section(&c);
        let mut ds = DataSection::new();
        ds.active(0, &ConstExpr::global_get(0), b"hello\0".iter().copied());
        m.section(&ds);
        m.section(&CustomSection { name: "producers".into(), data: b"\0".as_slice().into() });
        m.finish()
    }

    #[test]
    fn exports_by_name() {
        let names: Vec<String> = (0..5000).map(|i| format!("sym{i}")).collect();
        let mut x: Exports = names.iter().enumerate().map(|(i, n)| (n.as_str(), Export { kind: Kind::Func, index: i as u32 })).collect();
        assert_eq!(x.len(), 5000);
        for (i, n) in names.iter().enumerate() {
            assert_eq!(x.get(n).map(|e| e.index), Some(i as u32));
            assert_eq!(x[n.as_str()].index, i as u32);
        }
        assert!(!x.contains_key("sym5000") && x.get("").is_none());
        // in the order added; an insert of a name there replaces its export
        assert!(x.iter().map(|(n, _)| n).eq(names.iter().map(String::as_str)));
        x.insert("sym7", Export { kind: Kind::Global, index: 1 });
        assert_eq!((x.len(), x["sym7"].kind), (5000, Kind::Global));
        assert!(Exports::default().get("a").is_none());
    }

    #[test]
    fn forkreach_roundtrip() {
        let f = ForkReach {
            may_fork: ["system".to_string(), "fork".to_string()]
                .into_iter()
                .collect(),
        };
        let d = ForkReach::decode(&f.encode()).unwrap();
        assert_eq!(d.may_fork, f.may_fork);
    }
}
