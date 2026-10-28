//! Relayout ([`gex_ir::relayout`]) of x86-64 code built for LP64 onto ILP32
//! data, with the layouts read from the program's DWARF.
//!
//! `tests/relayout/relayout.c` (built by `tests/relayout/build.sh`) walks
//! structs holding `long`s and pointers: a list, a pair of lists, an array
//! of pairs, an array of node pointers, a global, a pointer difference.
//! Each function is recovered, relaid out, and run in the GIR interpreter
//! twice: as built, on the data laid out for LP64, and relaid out, on the
//! same data laid out for ILP32 below 4 GiB. Both runs must return the
//! same thing (the same integer, or the address of the same object) and
//! leave the data the same.

#![cfg(feature = "x86")]

use gex_ir::func::{Callee, Function};
use gex_ir::interp::{initial_regs, run_with_memory, Model, Outcome, Stop};
use gex_ir::relayout::{
    relayout, Abi, Field, Global, Kind, Layout, LayoutId, LayoutOracle, Report, Shape,
};
use gex_ir::ssa::CallEffect;
use gex_ir::GuestReg;
use gex_xlate::func::{recover, RecoverOptions};
use std::collections::{BTreeMap, HashMap};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/relayout/relayout.elf");

// ---------------------------------------------------------------------------
// A layout oracle from DWARF
// ---------------------------------------------------------------------------

/// One DIE, with what the oracle needs of it.
#[derive(Debug)]
struct Die {
    tag: gimli::DwTag,
    name: Option<String>,
    byte_size: Option<u64>,
    ty: Option<usize>,
    member_off: Option<u64>,
    encoding: Option<gimli::DwAte>,
    low_pc: Option<u64>,
    addr: Option<u64>,
    count: Option<u64>,
    children: Vec<usize>,
}

impl Default for Die {
    fn default() -> Die {
        Die {
            tag: gimli::DW_TAG_null,
            name: None,
            byte_size: None,
            ty: None,
            member_off: None,
            encoding: None,
            low_pc: None,
            addr: None,
            count: None,
            children: Vec::new(),
        }
    }
}

struct Dwarf {
    layouts: Vec<Layout>,
    /// Layout of each DIE turned into one (structs, and pointees).
    by_die: HashMap<usize, LayoutId>,
    funcs: HashMap<u64, (Vec<Shape>, Option<Shape>)>,
    globals: Vec<(Global, u64)>,
    /// Name to (source address, layout).
    global_names: HashMap<String, Global>,
    ptr_pointee: HashMap<usize, Option<LayoutId>>,
}

fn read_dies(bytes: &[u8]) -> BTreeMap<usize, Die> {
    use gimli::{AttributeValue, EndianSlice, RunTimeEndian};
    use object::{Object, ObjectSection};
    let file = object::File::parse(bytes).unwrap();
    let load = |id: gimli::SectionId| -> Result<std::borrow::Cow<[u8]>, gimli::Error> {
        Ok(file
            .section_by_name(id.name())
            .and_then(|s| s.uncompressed_data().ok())
            .unwrap_or(std::borrow::Cow::Borrowed(&[])))
    };
    let cow = gimli::DwarfSections::load(load).unwrap();
    let dwarf = cow.borrow(|s| EndianSlice::new(s, RunTimeEndian::Little));
    let mut dies = BTreeMap::new();
    let mut units = dwarf.units();
    let mut index = 0usize;
    while let Some(h) = units.next().unwrap() {
        // Unit-relative offsets, made unique across units.
        let base = index << 32;
        index += 1;
        let unit = dwarf.unit(h).unwrap();
        let mut tree = unit.entries_tree(None).unwrap();
        fn walk(
            dwarf: &gimli::Dwarf<EndianSlice<RunTimeEndian>>,
            unit: &gimli::Unit<EndianSlice<RunTimeEndian>>,
            base: usize,
            node: gimli::EntriesTreeNode<EndianSlice<RunTimeEndian>>,
            dies: &mut BTreeMap<usize, Die>,
        ) -> usize {
            let e = node.entry();
            let at = base + e.offset().0;
            let mut d = Die {
                tag: e.tag(),
                ..Default::default()
            };
            if let Some(v) = e.attr_value(gimli::DW_AT_name) {
                d.name = dwarf
                    .attr_string(unit, v)
                    .ok()
                    .map(|s| s.to_string_lossy().into_owned());
            }
            d.byte_size = e.attr(gimli::DW_AT_byte_size).and_then(|a| a.udata_value());
            if let Some(AttributeValue::UnitRef(o)) = e.attr_value(gimli::DW_AT_type) {
                d.ty = Some(base + o.0);
            }
            d.member_off = e
                .attr(gimli::DW_AT_data_member_location)
                .and_then(|a| a.udata_value());
            if let Some(AttributeValue::Encoding(x)) = e.attr_value(gimli::DW_AT_encoding) {
                d.encoding = Some(x);
            }
            if let Some(AttributeValue::Addr(a)) = e.attr_value(gimli::DW_AT_low_pc) {
                d.low_pc = Some(a);
            }
            if let Some(v @ AttributeValue::DebugAddrIndex(_)) = e.attr_value(gimli::DW_AT_low_pc) {
                d.low_pc = dwarf.attr_address(unit, v).ok().flatten();
            }
            if let Some(AttributeValue::Exprloc(x)) = e.attr_value(gimli::DW_AT_location) {
                let s = x.0.slice();
                if s.len() == 9 && s[0] == 0x03 {
                    d.addr = Some(u64::from_le_bytes(s[1..9].try_into().unwrap()));
                }
                if s.len() >= 2 && s[0] == 0xa1 {
                    // DW_OP_addrx
                    let mut r = gimli::EndianSlice::new(&s[1..], RunTimeEndian::Little);
                    let i = gimli::Reader::read_uleb128(&mut r).unwrap();
                    d.addr = dwarf.address(unit, gimli::DebugAddrIndex(i as usize)).ok();
                }
            }
            d.count = e
                .attr(gimli::DW_AT_count)
                .and_then(|a| a.udata_value())
                .or_else(|| {
                    e.attr(gimli::DW_AT_upper_bound)
                        .and_then(|a| a.udata_value())
                        .map(|u| u + 1)
                });
            let mut kids = node.children();
            while let Some(k) = kids.next().unwrap() {
                let c = walk(dwarf, unit, base, k, dies);
                d.children.push(c);
            }
            dies.insert(at, d);
            at
        }
        let root = tree.root().unwrap();
        walk(&dwarf, &unit, base, root, &mut dies);
    }
    dies
}

impl Dwarf {
    fn new(bytes: &[u8]) -> Dwarf {
        let dies = read_dies(bytes);
        let mut o = Dwarf {
            layouts: Vec::new(),
            by_die: HashMap::new(),
            funcs: HashMap::new(),
            globals: Vec::new(),
            global_names: HashMap::new(),
            ptr_pointee: HashMap::new(),
        };
        let mut dst_next = 0x2000_0000u64;
        for (&at, d) in &dies {
            if d.tag == gimli::DW_TAG_subprogram {
                if let Some(pc) = d.low_pc {
                    let args = d
                        .children
                        .iter()
                        .filter(|c| dies[c].tag == gimli::DW_TAG_formal_parameter)
                        .map(|c| o.shape(&dies, dies[c].ty))
                        .collect();
                    let ret = d.ty.map(|t| o.shape(&dies, Some(t)));
                    o.funcs.insert(pc, (args, ret));
                }
            }
            if d.tag == gimli::DW_TAG_variable {
                if let (Some(a), Some(t)) = (d.addr, d.ty) {
                    let l = o.layout_of(&dies, t);
                    let (src_size, dst_size, align) = {
                        let lay = &o.layouts[l as usize];
                        (lay.src_size, lay.dst_size, o.dst_align_layout(l))
                    };
                    dst_next = dst_next.next_multiple_of(align.max(1));
                    let g = Global {
                        src: a,
                        dst: dst_next,
                        layout: l,
                    };
                    dst_next += dst_size;
                    o.globals.push((g, src_size));
                    if let Some(n) = &d.name {
                        o.global_names.insert(n.clone(), g);
                    }
                }
            }
            let _ = at;
        }
        o
    }

    fn strip(dies: &BTreeMap<usize, Die>, mut t: usize) -> usize {
        while matches!(
            dies[&t].tag,
            gimli::DW_TAG_typedef | gimli::DW_TAG_const_type | gimli::DW_TAG_volatile_type
        ) {
            match dies[&t].ty {
                Some(n) => t = n,
                None => break,
            }
        }
        t
    }

    fn shape(&mut self, dies: &BTreeMap<usize, Die>, t: Option<usize>) -> Shape {
        let Some(t) = t else { return Shape::Int };
        let t = Self::strip(dies, t);
        if dies[&t].tag == gimli::DW_TAG_pointer_type {
            Shape::Ptr(self.pointee(dies, t))
        } else {
            Shape::Int
        }
    }

    fn pointee(&mut self, dies: &BTreeMap<usize, Die>, ptr: usize) -> Option<LayoutId> {
        if let Some(p) = self.ptr_pointee.get(&ptr) {
            return *p;
        }
        let p = dies[&ptr].ty.and_then(|t| {
            let t = Self::strip(dies, t);
            // void (no type) and functions have no layout.
            (dies[&t].tag != gimli::DW_TAG_subroutine_type).then(|| self.layout_of(dies, t))
        });
        self.ptr_pointee.insert(ptr, p);
        p
    }

    /// The kind of a member or element of type `t`.
    fn kind(&mut self, dies: &BTreeMap<usize, Die>, t: usize) -> Kind {
        let t = Self::strip(dies, t);
        let d = &dies[&t];
        match d.tag {
            gimli::DW_TAG_pointer_type => Kind::Ptr(self.pointee(dies, t)),
            gimli::DW_TAG_base_type | gimli::DW_TAG_enumeration_type => {
                let src = d.byte_size.unwrap() as u32;
                let name = d.name.clone().unwrap_or_default();
                // LP64 to ILP32: `long` and `unsigned long` shrink.
                let dst = if src == 8 && name.contains("long") && !name.contains("long long") {
                    4
                } else {
                    src
                };
                let signed = matches!(
                    d.encoding,
                    Some(gimli::DW_ATE_signed | gimli::DW_ATE_signed_char)
                );
                Kind::Scalar { src, dst, signed }
            }
            gimli::DW_TAG_array_type => {
                let elem = self.layout_of(dies, d.ty.unwrap());
                let count = d.children.iter().find_map(|c| dies[c].count).unwrap_or(0);
                Kind::Array { elem, count }
            }
            _ => Kind::Inline(self.layout_of(dies, t)),
        }
    }

    fn sizes(&self, k: Kind) -> (u64, u64) {
        match k {
            Kind::Scalar { src, dst, .. } => (src as u64, dst as u64),
            Kind::Ptr(_) => (8, 4),
            Kind::Inline(l) => (
                self.layouts[l as usize].src_size,
                self.layouts[l as usize].dst_size,
            ),
            Kind::Array { elem, count } => {
                let l = &self.layouts[elem as usize];
                (l.src_size * count, l.dst_size * count)
            }
        }
    }

    /// wasm32 alignment.
    fn dst_align(&self, k: Kind) -> u64 {
        match k {
            Kind::Scalar { dst, .. } => dst as u64,
            Kind::Ptr(_) => 4,
            Kind::Inline(l) => self.dst_align_layout(l),
            Kind::Array { elem, .. } => self.dst_align_layout(elem),
        }
    }

    fn dst_align_layout(&self, l: LayoutId) -> u64 {
        self.layouts[l as usize]
            .fields
            .iter()
            .map(|f| self.dst_align(f.kind))
            .max()
            .unwrap_or(1)
    }

    /// The layout of a struct, or a one-field one for anything else (the
    /// pointee of a `char *` or a `long *`).
    fn layout_of(&mut self, dies: &BTreeMap<usize, Die>, t: usize) -> LayoutId {
        let t = Self::strip(dies, t);
        if let Some(l) = self.by_die.get(&t) {
            return *l;
        }
        let id = self.layouts.len() as LayoutId;
        self.by_die.insert(t, id);
        self.layouts.push(Layout {
            name: String::new(),
            src_size: 0,
            dst_size: 0,
            fields: Vec::new(),
        });
        let d = &dies[&t];
        let name = d.name.clone().unwrap_or_else(|| format!("die{t:#x}"));
        let mut fields = Vec::new();
        let (src_size, dst_size);
        if d.tag == gimli::DW_TAG_structure_type {
            let mut dst = 0u64;
            let mut align = 1;
            for c in d.children.clone() {
                let m = &dies[&c];
                if m.tag != gimli::DW_TAG_member {
                    continue;
                }
                let k = self.kind(dies, m.ty.unwrap());
                let a = self.dst_align(k);
                align = align.max(a);
                dst = dst.next_multiple_of(a);
                fields.push(Field {
                    src_off: m.member_off.unwrap_or(0),
                    dst_off: dst,
                    kind: k,
                });
                dst += self.sizes(k).1;
            }
            src_size = d.byte_size.unwrap_or(0);
            dst_size = dst.next_multiple_of(align);
        } else {
            let k = self.kind(dies, t);
            let (s, ds) = self.sizes(k);
            fields.push(Field {
                src_off: 0,
                dst_off: 0,
                kind: k,
            });
            src_size = s;
            dst_size = ds;
        }
        self.layouts[id as usize] = Layout {
            name,
            src_size,
            dst_size,
            fields,
        };
        id
    }

    fn field(&self, l: LayoutId, i: usize) -> (Field, u64, u64) {
        let f = self.layouts[l as usize].fields[i].clone();
        let (s, d) = self.sizes(f.kind);
        (f, s, d)
    }
}

const ARGS: [GuestReg; 6] = [
    GuestReg::X(7),
    GuestReg::X(6),
    GuestReg::X(2),
    GuestReg::X(1),
    GuestReg::X(8),
    GuestReg::X(9),
];

impl LayoutOracle for Dwarf {
    fn layout(&self, id: LayoutId) -> &Layout {
        &self.layouts[id as usize]
    }
    fn args(&self, entry: u64) -> Vec<(GuestReg, Shape)> {
        self.funcs
            .get(&entry)
            .map(|(a, _)| ARGS.iter().copied().zip(a.iter().copied()).collect())
            .unwrap_or_default()
    }
    fn returns(&self, entry: u64) -> Option<Shape> {
        self.funcs.get(&entry).and_then(|(_, r)| *r)
    }
    fn global(&self, addr: u64) -> Option<Global> {
        self.globals
            .iter()
            .find(|(g, n)| addr >= g.src && addr < g.src + n)
            .map(|(g, _)| *g)
    }
}

// ---------------------------------------------------------------------------
// The data, in either layout
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Src,
    Dst,
}

/// The test's objects: four nodes in an array, a string, a pair, three
/// pairs in an array, a table of three node pointers, and the global.
struct World {
    side: Side,
    mem: HashMap<u64, u8>,
    nodes: u64,
    name: u64,
    pair: u64,
    pairs: u64,
    tab: u64,
    head: u64,
    node_size: u64,
}

const KEYS: [i64; 4] = [10, 20, -30, 40];

impl World {
    fn new(o: &Dwarf, side: Side, node_l: LayoutId, pair_l: LayoutId) -> World {
        let base = match side {
            Side::Src => 0x5555_0000_0000u64,
            Side::Dst => 0x1000_0000u64,
        };
        let ps = if side == Side::Src { 8 } else { 4 };
        let size = |l: LayoutId| {
            let l = &o.layouts[l as usize];
            if side == Side::Src {
                l.src_size
            } else {
                l.dst_size
            }
        };
        let head = o.global_names["head"];
        let mut w = World {
            side,
            mem: HashMap::new(),
            nodes: base,
            name: base + 0x1000,
            pair: base + 0x2000,
            pairs: base + 0x3000,
            tab: base + 0x4000,
            head: if side == Side::Src {
                head.src
            } else {
                head.dst
            },
            node_size: size(node_l),
        };
        for (i, b) in b"abc\0".iter().enumerate() {
            w.mem.insert(w.name + i as u64, *b);
        }
        let node = |i: usize| w.nodes + i as u64 * size(node_l);
        let n: Vec<u64> = (0..4).map(node).collect();
        for i in 0..4 {
            let next = if i < 3 { n[i + 1] } else { 0 };
            w.put(o, node_l, n[i], 0, KEYS[i] as u64);
            w.put(o, node_l, n[i], 1, next);
            w.put(o, node_l, n[i], 2, w.name);
            w.put(o, node_l, n[i], 3, 7);
        }
        w.put(o, pair_l, w.pair, 0, n[0]);
        w.put(o, pair_l, w.pair, 1, n[2]);
        w.put(o, pair_l, w.pair, 2, 1000);
        for i in 0..3u64 {
            let p = w.pairs + i * size(pair_l);
            w.put(o, pair_l, p, 0, n[i as usize]);
            w.put(o, pair_l, p, 1, n[3 - i as usize]);
            w.put(o, pair_l, p, 2, i);
        }
        for (i, &x) in [n[3], n[1], n[0]].iter().enumerate() {
            w.put_bytes(w.tab + i as u64 * ps, x, ps);
        }
        let h = w.head;
        w.put(o, head.layout, h, 1, n[2]);
        w
    }

    fn put_bytes(&mut self, at: u64, v: u64, n: u64) {
        for i in 0..n {
            self.mem.insert(at + i, (v >> (8 * i)) as u8);
        }
    }

    fn slot(&self, o: &Dwarf, l: LayoutId, i: usize) -> (u64, u64) {
        let (f, s, d) = o.field(l, i);
        match self.side {
            Side::Src => (f.src_off, s),
            Side::Dst => (f.dst_off, d),
        }
    }

    fn put(&mut self, o: &Dwarf, l: LayoutId, obj: u64, i: usize, v: u64) {
        let (off, n) = self.slot(o, l, i);
        self.put_bytes(obj + off, v, n);
    }

    /// Field `i` of the object at `obj` after a run, sign-extended.
    fn get(&self, o: &Dwarf, out: &Outcome, l: LayoutId, obj: u64, i: usize) -> i64 {
        let (off, n) = self.slot(o, l, i);
        let mut v = 0u64;
        for k in 0..n {
            let a = obj + off + k;
            let b = out
                .memory
                .get(&a)
                .or(self.mem.get(&a))
                .copied()
                .unwrap_or(0);
            v |= (b as u64) << (8 * k);
        }
        let sh = 64 - 8 * n;
        ((v << sh) as i64) >> sh
    }

    /// Which object an address names, so results compare across layouts.
    fn object(&self, a: u64) -> String {
        if a == 0 {
            return "null".into();
        }
        if a >= self.nodes && a < self.nodes + 4 * self.node_size {
            let i = (a - self.nodes) / self.node_size;
            let r = (a - self.nodes) % self.node_size;
            return format!("node[{i}]+{r}");
        }
        if a == self.name {
            return "name".into();
        }
        format!("{a:#x}")
    }
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

struct Prog {
    o: Dwarf,
    funcs: BTreeMap<u64, Function>,
    syms: HashMap<String, u64>,
    node_l: LayoutId,
    pair_l: LayoutId,
}

fn prog() -> Prog {
    let bytes = std::fs::read(FIXTURE).unwrap();
    let image = gex_ld::elf::load(&bytes, 0).unwrap();
    let p = recover(&image, Some(&bytes), &RecoverOptions::default());
    let syms = image
        .symbols
        .iter()
        .map(|s| (s.name.clone(), s.value))
        .collect::<HashMap<_, _>>();
    let o = Dwarf::new(&bytes);
    let find = |n: &str| o.layouts.iter().position(|l| l.name == n).unwrap() as LayoutId;
    let (node_l, pair_l) = (find("node"), find("pair"));
    Prog {
        funcs: p.functions,
        syms,
        node_l,
        pair_l,
        o,
    }
}

fn model() -> Model<'static> {
    Model {
        call: &|_: Callee| CallEffect::UNKNOWN,
        sp: GuestReg::X(4),
        max_steps: 1000,
        syscall: None,
        narrow: false,
    }
}

/// Run `f` as built and relaid out with arguments built by `args` in each
/// world; returns both worlds and outcomes, and the relayout report.
fn both(
    p: &Prog,
    name: &str,
    ssa: bool,
    args: impl Fn(&World) -> Vec<u64>,
) -> (Report, [(World, Outcome); 2]) {
    let entry = p.syms[name];
    let mut f = p.funcs[&entry].clone();
    if ssa {
        f = gex_xlate::func::to_ssa(&f, &|_| CallEffect::UNKNOWN);
    }
    let orig = f.clone();
    let rep = relayout(&mut f, &p.o, &Abi::X86_64_SYSV);
    f.verify().unwrap();
    let mut res = Vec::new();
    for (side, func) in [(Side::Src, &orig), (Side::Dst, &f)] {
        let w = World::new(&p.o, side, p.node_l, p.pair_l);
        let mut regs = initial_regs(7, GuestReg::X(4));
        for (r, v) in ARGS.iter().zip(args(&w)) {
            regs[gex_ir::func::reg_index(*r)] = v;
        }
        let out = run_with_memory(func, 7, regs, &model(), &w.mem);
        assert!(
            matches!(out.stop, Stop::Return(_)),
            "{name}: {:?}",
            out.stop
        );
        res.push((w, out));
    }
    let b = res.pop().unwrap();
    let a = res.pop().unwrap();
    assert_eq!(a.1.stop, b.1.stop, "{name}");
    assert!(rep.untyped.is_empty(), "{name}: {:#?}", rep.untyped);
    (rep, [a, b])
}

fn rax(o: &Outcome) -> u64 {
    o.regs[gex_ir::func::reg_index(GuestReg::X(0))]
}

#[test]
fn dwarf_layouts_shrink_pointers_and_longs() {
    let p = prog();
    let node = &p.o.layouts[p.node_l as usize];
    let offs: Vec<(u64, u64)> = node.fields.iter().map(|f| (f.src_off, f.dst_off)).collect();
    assert_eq!(offs, [(0, 0), (8, 4), (16, 8), (24, 12)]);
    assert_eq!((node.src_size, node.dst_size), (32, 16));
    let pair = &p.o.layouts[p.pair_l as usize];
    assert_eq!((pair.src_size, pair.dst_size), (24, 12));
}

fn check_all(ssa: bool) {
    let p = prog();
    let o = &p.o;
    let ints = |name: &str, args: &dyn Fn(&World) -> Vec<u64>, want: i64| {
        let (rep, [(_, a), (_, b)]) = both(&p, name, ssa, args);
        assert_eq!(rax(&a) as i64, want, "{name} as built");
        assert_eq!(rax(&b) as i64, want, "{name} relaid out: {rep:?}");
    };
    let ptrs = |name: &str, args: &dyn Fn(&World) -> Vec<u64>, want: &str| {
        let (rep, [(wa, a), (wb, b)]) = both(&p, name, ssa, args);
        assert_eq!(wa.object(rax(&a)), want, "{name} as built");
        assert_eq!(wb.object(rax(&b)), want, "{name} relaid out: {rep:?}");
        assert!(rax(&b) < 1 << 32);
    };
    let node = |w: &World, i: u64| w.nodes + i * w.node_size;

    ints("sum_keys", &|w| vec![node(w, 0)], 40);
    ints("weight_of", &|w| vec![w.pair], 1000 + 10 + 30);
    ints("pair_key", &|w| vec![w.pairs, 2], 20);
    ints("count_between", &|w| vec![node(w, 0), node(w, 3)], 3);
    ptrs("name_of", &|w| vec![w.pair], "name");
    ptrs("nth", &|w| vec![w.tab, 1], "node[1]+0");
    ptrs("first", &|_| vec![], "node[2]+0");
    ptrs("second", &|w| vec![node(w, 1)], "node[2]+0");

    let (_, worlds) = both(&p, "set_tag", ssa, |w| vec![node(w, 1), 99]);
    for (w, out) in &worlds {
        let tags: Vec<i64> = (0..4)
            .map(|i| w.get(o, out, p.node_l, node(w, i), 3))
            .collect();
        assert_eq!(tags, [7, 99, 99, 99], "{:?}", w.side);
        let keys: Vec<i64> = (0..4)
            .map(|i| w.get(o, out, p.node_l, node(w, i), 0))
            .collect();
        assert_eq!(keys, KEYS, "{:?}", w.side);
    }
    let (_, worlds) = both(&p, "link_nodes", ssa, |w| vec![node(w, 3), node(w, 0)]);
    for (w, out) in &worlds {
        let next: Vec<String> = (0..4)
            .map(|i| w.object(w.get(o, out, p.node_l, node(w, i), 1) as u64))
            .collect();
        assert_eq!(
            next,
            ["null", "node[2]+0", "node[3]+0", "node[0]+0"],
            "{:?}",
            w.side
        );
    }
}

#[test]
fn relaid_out_functions_run_the_same_on_ilp32_data() {
    check_all(false);
}

#[test]
fn relaid_out_functions_run_the_same_in_ssa_form() {
    check_all(true);
}

/// `third_key` loads through what a callee returns: the interpreter models
/// calls, so this checks the rewrite itself: the key is a `long`, read as 4
/// sign-extended bytes at offset 0.
#[test]
fn a_callees_returned_pointer_is_typed() {
    let p = prog();
    let mut f = p.funcs[&p.syms["third_key"]].clone();
    let rep = relayout(&mut f, &p.o, &Abi::X86_64_SYSV);
    assert!(rep.untyped.is_empty(), "{:#?}", rep.untyped);
    assert_eq!(rep.accesses, 1, "{rep:?}");
    let loads: Vec<_> = f
        .blocks
        .iter()
        .flat_map(|b| &b.stmts)
        .filter_map(|s| match s {
            gex_ir::Stmt::Def {
                expr:
                    gex_ir::Expr::Load {
                        offset, sz, signed, ..
                    },
                ..
            } => Some((*offset, *sz, *signed)),
            _ => None,
        })
        .collect();
    assert!(loads.contains(&(0, gex_ir::MemSz::B4, true)), "{loads:?}");
}

/// The check has teeth: as built, the code misreads the ILP32 data, and the
/// pass reports what it changed.
#[test]
fn unrelaid_code_misreads_ilp32_data() {
    let p = prog();
    let f = &p.funcs[&p.syms["sum_keys"]];
    let w = World::new(&p.o, Side::Dst, p.node_l, p.pair_l);
    let mut regs = initial_regs(7, GuestReg::X(4));
    regs[gex_ir::func::reg_index(GuestReg::X(7))] = w.nodes;
    let out = run_with_memory(f, 7, regs, &model(), &w.mem);
    assert_ne!(rax(&out) as i64, 40);
    let mut g = f.clone();
    let rep = relayout(&mut g, &p.o, &Abi::X86_64_SYSV);
    // `n->key` and `n->next`, each a narrowed load.
    assert_eq!((rep.accesses, rep.untyped.len()), (2, 0), "{rep:?}");
}
