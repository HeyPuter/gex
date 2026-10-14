//! Function-level splitting of a linked unit (`gex-cgen-image 3`,
//! docs/function-tier.md §11), after the link: the functions a recorded run
//! called stay in the unit's module (the *primary*), every other function
//! goes to a *cold cluster*, a module of its own fetched the first time one
//! of its functions is called. Nothing is compiled again and nothing is
//! translated at run time: a cold function's code is clang's, moved.
//!
//! A crate of its own, not a gex-cgen module: a unit's cache key hashes
//! gex-cgen's sources (its `build.rs`), and the split, a step after the
//! link that changes nothing a unit is built from, must not re-key every
//! unit (it did, and an ejected page missed all 59 of Paint's units).
//!
//! A unit's module is position-independent: it imports the memory, the
//! process's function table, `__table_base` and a few host functions, and its
//! one element segment fills the table from `__table_base` with every guest
//! function's body (`f_<entry>`, all of one type). A function pointer is a
//! table index, so the split keeps every slot where it was:
//!
//! * The primary's element segment is the unit's, slot for slot. A cold
//!   slot holds a *stub* of the body type: it calls the import `gex.cold`
//!   with its cluster's number, then tail-calls its own slot, which the
//!   cluster's instantiation has filled with the real body.
//! * A cluster imports what the unit imports and fills its slots at
//!   instantiation (passive segments, `table.init` from its start function).
//! * A direct call or tail call to a guest function the module does not hold
//!   becomes an indirect one through that function's slot
//!   (`__table_base + slot`). Calls between functions a module holds stay
//!   direct. Helpers that are not guest functions (`fl_` leaf copies,
//!   `gex_locked_misaligned`) are copied into every module that calls them.
//! * `gex_refs_*` (exports that only take the bodies' addresses, never
//!   called) are dropped.
//!
//! The host publishes the primary with the unit's entry list unchanged, so
//! the shim's ftab, the bridges and the dispatch directory see the whole
//! unit; only the JS that instantiates it supplies `gex.cold`.
//!
//! [`instrument`] is the recording side: the whole unit, each guest function
//! calling the import `gexcov.hit(slot)` on entry, so a run can say which
//! functions it called.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use wasm_encoder::reencode::{Reencode, RoundtripReencoder};
use wasm_encoder::{
    CodeSection, ConstExpr, CustomSection, ElementSection, Elements, EntityType, Function, FunctionSection,
    ImportSection, Instruction as I, Module, NameMap, NameSection, StartSection, TypeSection, ValType,
};
use wasmparser::{Operator, Parser, Payload, TypeRef};

/// Import module and name of the cold-cluster loader a primary calls.
pub const COLD_MODULE: &str = "gex";
pub const COLD_NAME: &str = "cold";
/// Import module and name an instrumented unit calls on each function entry.
pub const COV_MODULE: &str = "gexcov";
pub const COV_NAME: &str = "hit";

/// A unit's module, parsed.
pub struct Unit<'a> {
    /// The type section's raw contents (copied as is) and its count.
    types_raw: &'a [u8],
    types: Vec<wasmparser::FuncType>,
    imports: Vec<(String, String, TypeRef)>,
    /// Function imports.
    nfi: u32,
    /// Each defined function's type.
    func_types: Vec<u32>,
    bodies: Vec<wasmparser::FunctionBody<'a>>,
    /// The element segment: slot -> function index.
    pub slots: Vec<u32>,
    /// Function index -> slot, for guest functions.
    slot_of: HashMap<u32, u32>,
    names: HashMap<u32, String>,
    /// The global `__table_base` is imported as.
    table_base: u32,
    /// The body type every slot holds.
    body_ty: u32,
    /// Raw custom sections to keep in the primary, in order (`dylink.0`
    /// first, `gex.funcs`).
    keep_custom: Vec<(String, &'a [u8])>,
    /// Each function's direct callees (function indices), in order.
    callees: Vec<Vec<u32>>,
}

impl<'a> Unit<'a> {
    pub fn parse(wasm: &'a [u8]) -> Result<Unit<'a>> {
        let mut u = Unit {
            types_raw: &[],
            types: Vec::new(),
            imports: Vec::new(),
            nfi: 0,
            func_types: Vec::new(),
            bodies: Vec::new(),
            slots: Vec::new(),
            slot_of: HashMap::new(),
            names: HashMap::new(),
            table_base: u32::MAX,
            body_ty: u32::MAX,
            keep_custom: Vec::new(),
            callees: Vec::new(),
        };
        let mut nglobals = 0u32;
        for payload in Parser::new(0).parse_all(wasm) {
            match payload? {
                Payload::TypeSection(r) => {
                    u.types_raw = &wasm[r.range()];
                    for rg in r.into_iter_err_on_gc_types() {
                        u.types.push(rg?);
                    }
                }
                Payload::ImportSection(r) => {
                    for imp in r {
                        let imp = imp?;
                        match imp.ty {
                            TypeRef::Func(_) => u.nfi += 1,
                            TypeRef::Global(_) => {
                                if imp.name == "__table_base" {
                                    u.table_base = nglobals;
                                }
                                nglobals += 1;
                            }
                            _ => {}
                        }
                        u.imports.push((imp.module.to_string(), imp.name.to_string(), imp.ty));
                    }
                }
                Payload::FunctionSection(r) => {
                    for t in r {
                        u.func_types.push(t?);
                    }
                }
                Payload::GlobalSection(_) => bail!("the unit defines globals (a split would give each module its own)"),
                Payload::DataSection(_) => bail!("the unit has data (a split would initialise it once per module)"),
                Payload::StartSection { .. } => bail!("the unit has a start function"),
                Payload::ElementSection(r) => {
                    for el in r {
                        let el = el?;
                        if !u.slots.is_empty() {
                            bail!("the unit has more than one element segment");
                        }
                        match el.kind {
                            wasmparser::ElementKind::Active { table_index, offset_expr } => {
                                if table_index.unwrap_or(0) != 0 {
                                    bail!("the element segment is not for table 0");
                                }
                                let mut ops = offset_expr.get_operators_reader();
                                match ops.read()? {
                                    Operator::GlobalGet { global_index } if global_index == u.table_base => {}
                                    other => bail!("the element segment's offset is {other:?}, not __table_base"),
                                }
                            }
                            _ => bail!("the element segment is not active"),
                        }
                        if let wasmparser::ElementItems::Functions(fs) = el.items {
                            for f in fs {
                                u.slots.push(f?);
                            }
                        } else {
                            bail!("the element segment holds expressions");
                        }
                    }
                }
                Payload::CodeSectionEntry(body) => u.bodies.push(body),
                Payload::CustomSection(c) => match c.name() {
                    "name" => {
                        let reader = wasmparser::NameSectionReader::new(wasmparser::BinaryReader::new(c.data(), c.data_offset()));
                        for n in reader.flatten() {
                            if let wasmparser::Name::Function(map) = n {
                                for naming in map.into_iter().flatten() {
                                    u.names.insert(naming.index, naming.name.to_string());
                                }
                            }
                        }
                    }
                    "dylink.0" | "gex.funcs" => u.keep_custom.push((c.name().to_string(), c.data())),
                    _ => {}
                },
                _ => {}
            }
        }
        if u.table_base == u32::MAX {
            bail!("the unit imports no __table_base");
        }
        if u.bodies.len() != u.func_types.len() {
            bail!("{} bodies for {} functions", u.bodies.len(), u.func_types.len());
        }
        for (k, &f) in u.slots.iter().enumerate() {
            if f < u.nfi {
                bail!("slot {k} holds an import");
            }
            let t = u.func_types[(f - u.nfi) as usize];
            if u.body_ty == u32::MAX {
                u.body_ty = t;
            } else if t != u.body_ty {
                bail!("slot {k} holds a function of another type");
            }
            if u.slot_of.insert(f, k as u32).is_some() {
                bail!("function {f} is in two slots");
            }
        }
        for b in &u.bodies {
            let mut cs = Vec::new();
            let mut ops = b.get_operators_reader()?;
            while !ops.eof() {
                match ops.read()? {
                    Operator::Call { function_index } | Operator::ReturnCall { function_index } => cs.push(function_index),
                    _ => {}
                }
            }
            u.callees.push(cs);
        }
        Ok(u)
    }

    /// Each slot's guest address (`gex.funcs`: a u32 count, a u64 each);
    /// the slot's index where the section is missing or short.
    pub fn addresses(&self) -> Vec<u64> {
        let d = self.keep_custom.iter().find(|(n, _)| n == "gex.funcs").map(|(_, d)| *d).unwrap_or(&[]);
        (0..self.slots.len())
            .map(|k| d.get(4 + 8 * k..12 + 8 * k).map_or(k as u64, |b| u64::from_le_bytes(b.try_into().unwrap())))
            .collect()
    }

    pub fn is_guest(&self, f: u32) -> bool {
        self.slot_of.contains_key(&f)
    }

    /// A function's direct callees.
    pub fn callees(&self, f: u32) -> &[u32] {
        &self.callees[(f - self.nfi) as usize]
    }

    /// Each slot's guest callees (slots) through direct calls, following the
    /// helpers it calls (they are copied into whatever module holds it).
    pub fn slot_callees(&self) -> Vec<Vec<u32>> {
        let mut out = Vec::with_capacity(self.slots.len());
        for &f in &self.slots {
            let mut seen = HashSet::new();
            let mut q = VecDeque::from([f]);
            let mut cs = Vec::new();
            while let Some(g) = q.pop_front() {
                for &c in self.callees(g) {
                    if c < self.nfi || !seen.insert(c) {
                        continue;
                    }
                    match self.slot_of.get(&c) {
                        Some(&s) => cs.push(s),
                        None => q.push_back(c),
                    }
                }
            }
            out.push(cs);
        }
        out
    }

    /// The size of slot `k`'s body in the unit, in bytes.
    pub fn body_size(&self, k: u32) -> usize {
        let f = self.slots[k as usize];
        self.bodies[(f - self.nfi) as usize].range().len()
    }

    pub fn name(&self, f: u32) -> Option<&str> {
        self.names.get(&f).map(|s| s.as_str())
    }

    /// The non-guest functions `roots` reach through direct calls (stopping
    /// at guest functions), in function-index order.
    fn helpers_of(&self, roots: &[u32]) -> Vec<u32> {
        let mut seen: HashSet<u32> = HashSet::new();
        let mut q: VecDeque<u32> = roots.iter().copied().collect();
        let mut out = Vec::new();
        while let Some(g) = q.pop_front() {
            for &c in self.callees(g) {
                if c < self.nfi || self.is_guest(c) || !seen.insert(c) {
                    continue;
                }
                out.push(c);
                q.push_back(c);
            }
        }
        out.sort_unstable();
        out
    }
}

/// What a module holds and how it is laid out.
struct Plan<'p> {
    /// Guest functions it holds, by slot.
    owned: &'p [u32],
    /// Primary: the cluster of each slot it does not own (stubs), and the
    /// whole element segment. `None`: a cluster.
    stubs: Option<&'p [Option<u32>]>,
    /// Instrumented: `gexcov.hit(slot)` on each guest function's entry.
    cov: bool,
}

fn build(u: &Unit, plan: &Plan) -> Result<Vec<u8>> {
    let owned_f: Vec<u32> = plan.owned.iter().map(|&k| u.slots[k as usize]).collect();
    let helpers = u.helpers_of(&owned_f);
    let need_cold = plan.stubs.is_some_and(|s| s.iter().any(|c| c.is_some()));
    let extra_import = need_cold || plan.cov;
    // Types: the unit's, then (i32) -> () for the extra import.
    let mut types = TypeSection::new();
    {
        let reader = wasmparser::TypeSectionReader::new(wasmparser::BinaryReader::new(u.types_raw, 0))?;
        RoundtripReencoder.parse_type_section(&mut types, reader).map_err(|e| anyhow!("types: {e:?}"))?;
    }
    let ty_i32 = u.types.len() as u32;
    if extra_import {
        types.ty().function([ValType::I32], []);
    }
    // Imports: the unit's, then the extra one.
    let mut imports = ImportSection::new();
    for (m, n, t) in &u.imports {
        let ty = RoundtripReencoder.entity_type(*t).map_err(|e| anyhow!("import {m}.{n}: {e:?}"))?;
        imports.import(m, n, ty);
    }
    let extra_idx = u.nfi;
    if need_cold {
        imports.import(COLD_MODULE, COLD_NAME, EntityType::Function(ty_i32));
    } else if plan.cov {
        imports.import(COV_MODULE, COV_NAME, EntityType::Function(ty_i32));
    }
    let first = u.nfi + extra_import as u32;
    // The function space: owned guest functions, helpers, then stubs and the
    // init function.
    let mut map: HashMap<u32, u32> = HashMap::new();
    for i in 0..u.nfi {
        map.insert(i, i);
    }
    let mut order: Vec<u32> = Vec::new();
    for &f in owned_f.iter().chain(helpers.iter()) {
        map.insert(f, first + order.len() as u32);
        order.push(f);
    }
    let mut funcs = FunctionSection::new();
    let mut code = CodeSection::new();
    let mut names = NameMap::new();
    let mut r = Remap { map: &map };
    for &f in &order {
        let d = (f - u.nfi) as usize;
        funcs.function(u.func_types[d]);
        let body = &u.bodies[d];
        let mut out = r.new_function_with_parsed_locals(body).map_err(|e| anyhow!("locals: {e:?}"))?;
        if plan.cov {
            if let Some(&k) = u.slot_of.get(&f) {
                out.instruction(&I::I32Const(k as i32));
                out.instruction(&I::Call(extra_idx));
            }
        }
        let mut ops = body.get_operators_reader()?;
        while !ops.eof() {
            let op = ops.read()?;
            match op {
                Operator::Call { function_index } | Operator::ReturnCall { function_index }
                    if function_index >= u.nfi && !map.contains_key(&function_index) =>
                {
                    let k = *u.slot_of.get(&function_index).ok_or_else(|| {
                        anyhow!("{} calls {}, which is neither held nor a guest function", u.name(f).unwrap_or("?"), u.name(function_index).unwrap_or("?"))
                    })?;
                    let tail = matches!(op, Operator::ReturnCall { .. });
                    out.instruction(&I::GlobalGet(u.table_base));
                    out.instruction(&I::I32Const(k as i32));
                    out.instruction(&I::I32Add);
                    if tail {
                        out.instruction(&I::ReturnCallIndirect { type_index: u.body_ty, table_index: 0 });
                    } else {
                        out.instruction(&I::CallIndirect { type_index: u.body_ty, table_index: 0 });
                    }
                }
                Operator::RefFunc { function_index } if !map.contains_key(&function_index) => {
                    bail!("{} takes the address of a function it does not hold", u.name(f).unwrap_or("?"));
                }
                op => {
                    out.instruction(&r.instruction(op).map_err(|e| anyhow!("{}: {e:?}", u.name(f).unwrap_or("?")))?);
                }
            }
        }
        code.function(&out);
        if let Some(n) = u.name(f) {
            names.append(map[&f], n);
        }
    }
    let mut next = first + order.len() as u32;
    let mut elems = ElementSection::new();
    let mut start = None;
    match plan.stubs {
        Some(stubs) => {
            // A stub per cold slot: load its cluster, then call the slot again.
            let mut seg = Vec::with_capacity(u.slots.len());
            for (k, &f) in u.slots.iter().enumerate() {
                match stubs[k] {
                    None => seg.push(*map.get(&f).ok_or_else(|| anyhow!("hot slot {k} is not held"))?),
                    Some(c) => {
                        funcs.function(u.body_ty);
                        let mut s = Function::new([]);
                        s.instruction(&I::I32Const(c as i32));
                        s.instruction(&I::Call(extra_idx));
                        for p in 0..4 {
                            s.instruction(&I::LocalGet(p));
                        }
                        s.instruction(&I::GlobalGet(u.table_base));
                        s.instruction(&I::I32Const(k as i32));
                        s.instruction(&I::I32Add);
                        s.instruction(&I::ReturnCallIndirect { type_index: u.body_ty, table_index: 0 });
                        s.instruction(&I::End);
                        code.function(&s);
                        seg.push(next);
                        next += 1;
                    }
                }
            }
            elems.active(None, &ConstExpr::global_get(u.table_base), Elements::Functions(seg.as_slice().into()));
        }
        None => {
            // A cluster: one passive segment per run of consecutive slots,
            // copied into the table from its start function.
            let mut ks: Vec<u32> = plan.owned.to_vec();
            ks.sort_unstable();
            let mut runs: Vec<(u32, Vec<u32>)> = Vec::new();
            for k in ks {
                let f = map[&u.slots[k as usize]];
                match runs.last_mut() {
                    Some((s, v)) if *s + v.len() as u32 == k => v.push(f),
                    _ => runs.push((k, vec![f])),
                }
            }
            let mut init = Function::new([]);
            for (i, (s, v)) in runs.iter().enumerate() {
                elems.passive(Elements::Functions(v.as_slice().into()));
                init.instruction(&I::GlobalGet(u.table_base));
                init.instruction(&I::I32Const(*s as i32));
                init.instruction(&I::I32Add);
                init.instruction(&I::I32Const(0));
                init.instruction(&I::I32Const(v.len() as i32));
                init.instruction(&I::TableInit { elem_index: i as u32, table: 0 });
            }
            init.instruction(&I::End);
            let ty_void = types.len();
            types.ty().function([], []);
            funcs.function(ty_void);
            code.function(&init);
            names.append(next, "gex_cold_init");
            start = Some(next);
        }
    }
    let mut m = Module::new();
    if plan.stubs.is_some() {
        if let Some((_, d)) = u.keep_custom.iter().find(|(n, _)| n == "dylink.0") {
            m.section(&CustomSection { name: "dylink.0".into(), data: (*d).into() });
        }
    }
    m.section(&types);
    m.section(&imports);
    m.section(&funcs);
    if let Some(s) = start {
        m.section(&StartSection { function_index: s });
    }
    m.section(&elems);
    m.section(&code);
    let mut ns = NameSection::new();
    ns.functions(&names);
    m.section(&ns);
    if plan.stubs.is_some() {
        if let Some((_, d)) = u.keep_custom.iter().find(|(n, _)| n == "gex.funcs") {
            m.section(&CustomSection { name: "gex.funcs".into(), data: (*d).into() });
        }
    }
    Ok(m.finish())
}

struct Remap<'m> {
    map: &'m HashMap<u32, u32>,
}

impl Reencode for Remap<'_> {
    type Error = std::convert::Infallible;
    fn function_index(&mut self, f: u32) -> u32 {
        *self.map.get(&f).expect("unmapped function index")
    }
}

/// The unit with `gexcov.hit(slot)` called on each guest function's entry.
pub fn instrument(wasm: &[u8]) -> Result<Vec<u8>> {
    let u = Unit::parse(wasm)?;
    let all: Vec<u32> = (0..u.slots.len() as u32).collect();
    build(&u, &Plan { owned: &all, stubs: Some(&vec![None; u.slots.len()]), cov: true })
}

/// How cold functions are grouped into what the page fetches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clustering {
    /// Every cold function a module of its own.
    Function,
    /// All of a unit's cold functions one module.
    Unit,
    /// By the call graph: each cold function the hot code (or another
    /// cluster) calls starts a cluster, which takes the cold functions it
    /// reaches that no earlier cluster took, up to `cap` bytes of code.
    Graph { cap: usize },
    /// By address: consecutive cold functions (an image's code from one
    /// source file sits together), a cluster per `cap` bytes of code.
    Chunk { cap: usize },
    /// The call graph's clusters (as `Graph`, same cap), then packed by
    /// address into clusters of up to `cap` bytes of code.
    GraphChunk { cap: usize },
}

impl std::str::FromStr for Clustering {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "function" | "fn" => Clustering::Function,
            "unit" | "dll" => Clustering::Unit,
            s => {
                let bad = || anyhow!("clustering {s:?}: function, unit, or graph, chunk or graphchunk[:KiB]");
                let (name, cap) = s.split_once(':').unwrap_or((s, ""));
                let cap = if cap.is_empty() { 64 * 1024 } else { cap.parse::<usize>().map_err(|_| bad())? * 1024 };
                match name {
                    "graph" => Clustering::Graph { cap },
                    "chunk" => Clustering::Chunk { cap },
                    "graphchunk" => Clustering::GraphChunk { cap },
                    _ => return Err(bad()),
                }
            }
        })
    }
}

/// Each cold slot's cluster (`None`: hot), and how many clusters.
pub fn clusters(u: &Unit, hot: &HashSet<u32>, how: Clustering) -> (Vec<Option<u32>>, u32) {
    let n = u.slots.len();
    let mut of: Vec<Option<u32>> = vec![None; n];
    let cold: Vec<u32> = (0..n as u32).filter(|k| !hot.contains(k)).collect();
    let mut count = 0u32;
    match how {
        Clustering::Function => {
            for k in cold {
                of[k as usize] = Some(count);
                count += 1;
            }
        }
        Clustering::Unit => {
            if !cold.is_empty() {
                for k in cold {
                    of[k as usize] = Some(0);
                }
                count = 1;
            }
        }
        Clustering::Chunk { cap } => {
            let mut size = 0usize;
            for k in by_address(u, &cold) {
                let s = u.body_size(k);
                if size > 0 && size + s > cap {
                    count += 1;
                    size = 0;
                }
                of[k as usize] = Some(count);
                size += s;
            }
            if size > 0 {
                count += 1;
            }
        }
        Clustering::GraphChunk { cap } => {
            let (g, gn) = clusters(u, hot, Clustering::Graph { cap });
            // Each graph cluster's size and lowest address, then bins in
            // address order.
            let mut size = vec![0usize; gn as usize];
            let mut low = vec![u64::MAX; gn as usize];
            let at = u.addresses();
            for (k, c) in g.iter().enumerate() {
                if let Some(c) = c {
                    size[*c as usize] += u.body_size(k as u32);
                    low[*c as usize] = low[*c as usize].min(at[k]);
                }
            }
            let mut order: Vec<u32> = (0..gn).collect();
            order.sort_by_key(|&c| low[c as usize]);
            let mut bin = vec![0u32; gn as usize];
            let mut fill = 0usize;
            for c in order {
                let s = size[c as usize];
                if fill > 0 && fill + s > cap {
                    count += 1;
                    fill = 0;
                }
                bin[c as usize] = count;
                fill += s;
            }
            if fill > 0 {
                count += 1;
            }
            for (k, c) in g.iter().enumerate() {
                of[k] = c.map(|c| bin[c as usize]);
            }
        }
        Clustering::Graph { cap } => {
            let calls = u.slot_callees();
            // Roots in order: cold functions the hot code calls directly,
            // then the rest by address (reached through the table: callbacks,
            // vtables, exports).
            let mut roots: Vec<u32> = Vec::new();
            let mut seen = HashSet::new();
            for k in 0..n as u32 {
                if hot.contains(&k) {
                    for &c in &calls[k as usize] {
                        if !hot.contains(&c) && seen.insert(c) {
                            roots.push(c);
                        }
                    }
                }
            }
            for &k in &cold {
                if seen.insert(k) {
                    roots.push(k);
                }
            }
            for r in roots {
                if of[r as usize].is_some() {
                    continue;
                }
                let c = count;
                count += 1;
                let mut size = 0usize;
                let mut q = VecDeque::from([r]);
                while let Some(k) = q.pop_front() {
                    if of[k as usize].is_some() {
                        continue;
                    }
                    let s = u.body_size(k);
                    if size > 0 && size + s > cap {
                        continue;
                    }
                    of[k as usize] = Some(c);
                    size += s;
                    for &d in &calls[k as usize] {
                        if !hot.contains(&d) && of[d as usize].is_none() {
                            q.push_back(d);
                        }
                    }
                }
            }
        }
    }
    (of, count)
}

/// Slots in order of their guest address.
fn by_address(u: &Unit, slots: &[u32]) -> Vec<u32> {
    let at = u.addresses();
    let mut v = slots.to_vec();
    v.sort_by_key(|&k| at[k as usize]);
    v
}

/// The split: the primary module, each cluster's module, and each slot's
/// cluster (`None`: in the primary).
pub fn split(wasm: &[u8], hot: &HashSet<u32>, how: Clustering) -> Result<(Vec<u8>, Vec<Vec<u8>>, Vec<Option<u32>>)> {
    let u = Unit::parse(wasm)?;
    let (of, count) = clusters(&u, hot, how);
    let hot_slots: Vec<u32> = (0..u.slots.len() as u32).filter(|k| of[*k as usize].is_none()).collect();
    let primary = build(&u, &Plan { owned: &hot_slots, stubs: Some(&of), cov: false })?;
    let mut members: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (k, c) in of.iter().enumerate() {
        if let Some(c) = c {
            members.entry(*c).or_default().push(k as u32);
        }
    }
    let mut out = Vec::with_capacity(count as usize);
    for c in 0..count {
        let ks = members.get(&c).map(|v| v.as_slice()).unwrap_or(&[]);
        out.push(build(&u, &Plan { owned: ks, stubs: None, cov: false })?);
    }
    Ok((primary, out, of))
}
