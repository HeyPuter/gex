//! Relayout: rewrite a function compiled for one data layout so that it
//! runs on the same data laid out another way, typically code built for
//! LP64 (8-byte pointers and `long`s) running on ILP32 data (wasm32).
//!
//! Registers stay as wide as the guest's. Only memory changes: every access
//! through a pointer whose pointee is known is moved to the field's offset
//! in the destination layout and given the field's destination width (a
//! pointer or a `long` loads 4 bytes, zero- or sign-extended, and stores its
//! low 4 bytes). Pointer arithmetic follows: a constant step moves by the
//! destination distance between the same two fields, an index scaled by the
//! source element size is rescaled to the destination's, and the difference
//! of two pointers is scaled back to source units so the code dividing it
//! by the element size still counts elements. Addresses of known globals
//! are moved to their destination addresses.
//!
//! What a pointer points to comes from a [`LayoutOracle`]: the layouts, the
//! shapes of the function's arguments and of what callees return, and the
//! globals. The pass propagates those shapes over the function's values and
//! registers (and 8-byte stack slots, which keep spilled pointers typed)
//! to a fixpoint, then rewrites. The stack frame keeps its layout: it is the
//! function's own. Whatever could not be typed and might need moving is
//! listed in the [`Report`], never guessed; a function with an empty
//! `untyped` list is fully relaid out.
//!
//! Works on either form ([`Form::Memory`] or [`Form::Ssa`]).

use crate::func::*;
use crate::*;
use std::collections::BTreeMap;

/// Index of a [`Layout`] in the oracle.
pub type LayoutId = u32;

/// One type's layout in both data models. Arrays of it are `src_size` and
/// `dst_size` apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub name: String,
    pub src_size: u64,
    pub dst_size: u64,
    /// By source offset.
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub src_off: u64,
    pub dst_off: u64,
    pub kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// An integer or float, `src` bytes in the source layout and `dst` in
    /// the destination one; when they differ, a narrowed load extends by
    /// `signed`.
    Scalar { src: u32, dst: u32, signed: bool },
    /// A pointer: 8 bytes in the source, 4 in the destination; `None` when
    /// what it points to is unknown.
    Ptr(Option<LayoutId>),
    /// An embedded struct.
    Inline(LayoutId),
    /// An embedded array.
    Array { elem: LayoutId, count: u64 },
}

/// What a register holds at a function boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Int,
    Ptr(Option<LayoutId>),
}

/// A global object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Global {
    pub src: u64,
    pub dst: u64,
    pub layout: LayoutId,
}

pub trait LayoutOracle {
    fn layout(&self, id: LayoutId) -> &Layout;
    /// The argument registers of the function at `entry` that matter.
    fn args(&self, entry: u64) -> Vec<(GuestReg, Shape)>;
    /// What the function at `entry` returns, when known.
    fn returns(&self, entry: u64) -> Option<Shape>;
    /// The global whose source extent contains `addr`.
    fn global(&self, addr: u64) -> Option<Global>;
}

/// The calling convention the pass needs.
#[derive(Clone, Copy, Debug)]
pub struct Abi {
    pub sp: GuestReg,
    /// The integer return register.
    pub ret: GuestReg,
    /// Registers a callee hands back unchanged.
    pub callee_saved: RegSet,
    /// Stack pointer after a call returns minus at the call terminator,
    /// whose block has already done the call's own stack effect (x86-64's
    /// `call` pushed the return address, which the callee's `ret` pops).
    pub call_sp_delta: i64,
    /// Integer argument registers, in order: a stack address passed in one
    /// escapes the frame.
    pub arg_regs: &'static [GuestReg],
}

impl Abi {
    /// x86-64 System V.
    pub const X86_64_SYSV: Abi = Abi {
        sp: GuestReg::X(4),
        ret: GuestReg::X(0),
        callee_saved: RegSet(
            1 << 3 | 1 << 5 | 1 << 12 | 1 << 13 | 1 << 14 | 1 << 15, // rbx rbp r12-r15
        ),
        call_sp_delta: 8,
        arg_regs: &[
            GuestReg::X(7),
            GuestReg::X(6),
            GuestReg::X(2),
            GuestReg::X(1),
            GuestReg::X(8),
            GuestReg::X(9),
        ],
    };
}

/// What the pass did and what it could not do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Memory accesses moved or resized.
    pub accesses: usize,
    /// Accesses through a typed pointer that needed nothing.
    pub unchanged: usize,
    /// Constant steps, rescaled indices and differences rewritten.
    pub arithmetic: usize,
    /// Global addresses moved.
    pub globals: usize,
    /// Guest pc and what is wrong, for every access or value that may need
    /// relayout and was not relaid out.
    pub untyped: Vec<(u64, String)>,
}

// ---------------------------------------------------------------------------
// Layout queries
// ---------------------------------------------------------------------------

fn kind_sizes(o: &dyn LayoutOracle, k: Kind) -> (u64, u64) {
    match k {
        Kind::Scalar { src, dst, .. } => (src as u64, dst as u64),
        Kind::Ptr(_) => (8, 4),
        Kind::Inline(l) => {
            let l = o.layout(l);
            (l.src_size, l.dst_size)
        }
        Kind::Array { elem, count } => {
            let l = o.layout(elem);
            (l.src_size * count, l.dst_size * count)
        }
    }
}

/// A scalar or pointer field: where it starts in both layouts.
#[derive(Clone, Copy, Debug)]
struct Leaf {
    src: i64,
    dst: i64,
    kind: Kind,
}

/// The leaf of `l` containing source offset `off` (any element of an array
/// of `l`), or `None` for padding.
fn leaf_at(o: &dyn LayoutOracle, l: LayoutId, off: i64) -> Option<Leaf> {
    let lay = o.layout(l);
    if lay.src_size == 0 {
        return None;
    }
    let k = off.div_euclid(lay.src_size as i64);
    let r = off.rem_euclid(lay.src_size as i64) as u64;
    let (sb, db) = (k * lay.src_size as i64, k * lay.dst_size as i64);
    for f in &lay.fields {
        let (ss, _) = kind_sizes(o, f.kind);
        if r < f.src_off || r >= f.src_off + ss {
            continue;
        }
        let (fs, fd) = (sb + f.src_off as i64, db + f.dst_off as i64);
        return match f.kind {
            Kind::Inline(m) => leaf_at(o, m, r as i64 - f.src_off as i64).map(|x| Leaf {
                src: fs + x.src,
                dst: fd + x.dst,
                kind: x.kind,
            }),
            Kind::Array { elem, .. } => {
                leaf_at(o, elem, r as i64 - f.src_off as i64).map(|x| Leaf {
                    src: fs + x.src,
                    dst: fd + x.dst,
                    kind: x.kind,
                })
            }
            k => Some(Leaf {
                src: fs,
                dst: fd,
                kind: k,
            }),
        };
    }
    None
}

/// The destination offset of source offset `off` in (an array of) `l`, when
/// it is a position the destination has: a leaf's start, a byte inside a
/// leaf of unchanged size, or the end of an element.
fn map_off(o: &dyn LayoutOracle, l: LayoutId, off: i64) -> Option<i64> {
    let lay = o.layout(l);
    if lay.src_size == 0 {
        return (off == 0).then_some(0);
    }
    if off.rem_euclid(lay.src_size as i64) == 0 {
        return Some(off.div_euclid(lay.src_size as i64) * lay.dst_size as i64);
    }
    let leaf = leaf_at(o, l, off)?;
    let (ss, ds) = kind_sizes(o, leaf.kind);
    let inner = off - leaf.src;
    (inner == 0 || ss == ds).then_some(leaf.dst + inner)
}

/// How an access of `sz` bytes at source offset `off` of `l` is done in
/// the destination.
struct Access {
    dst: i64,
    sz: MemSz,
    /// Extension of a narrowed load.
    signed: Option<bool>,
    /// What a load produces.
    result: T,
}

fn access(o: &dyn LayoutOracle, l: LayoutId, off: i64, sz: u32) -> Result<Access, String> {
    let leaf = leaf_at(o, l, off).ok_or_else(|| format!("{}+{off}: padding", o.layout(l).name))?;
    let inner = off - leaf.src;
    let (ss, ds) = kind_sizes(o, leaf.kind);
    let msz = |n: u64| match n {
        1 => MemSz::B1,
        2 => MemSz::B2,
        4 => MemSz::B4,
        _ => MemSz::B8,
    };
    match leaf.kind {
        Kind::Ptr(p) if inner == 0 && sz == 8 => Ok(Access {
            dst: leaf.dst,
            sz: MemSz::B4,
            signed: Some(false),
            result: T::Ptr { l: p, d: 0 },
        }),
        Kind::Scalar { signed, .. } if inner == 0 && sz as u64 == ss && ds < ss => Ok(Access {
            dst: leaf.dst,
            sz: msz(ds),
            signed: Some(signed),
            result: T::Int,
        }),
        // The low part of a narrowed field, or any part of an unchanged one.
        _ if (inner == 0 && sz as u64 <= ds) || (ss == ds && inner + sz as i64 <= ss as i64) => {
            Ok(Access {
                dst: leaf.dst + inner,
                sz: msz(sz as u64),
                signed: None,
                result: T::Int,
            })
        }
        // Several unchanged leaves at unchanged distances.
        _ => {
            let mut pos = off;
            while pos < off + sz as i64 {
                let x = leaf_at(o, l, pos)
                    .ok_or_else(|| format!("{}+{pos}: padding", o.layout(l).name))?;
                let (a, b) = kind_sizes(o, x.kind);
                if a != b || (x.dst - (leaf.dst + inner)) != (x.src - off) {
                    return Err(format!(
                        "{}+{off}: {sz}-byte access spans fields that move",
                        o.layout(l).name
                    ));
                }
                pos = x.src + a as i64;
            }
            Ok(Access {
                dst: leaf.dst + inner,
                sz: msz(sz as u64),
                signed: None,
                result: T::Int,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// The lattice
// ---------------------------------------------------------------------------

/// What a value or register is known to hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum T {
    /// An integer that is not an address the pass knows.
    Int,
    /// Zero: joins with a pointer as that pointer.
    Null,
    /// A constant that is not a known global's address.
    Const(u64),
    /// A pointer into (an array of) layout `l`, at source offset `d` from
    /// an element's start (kept in `0..src_size`). `l` is `None` for a
    /// pointer to something unknown.
    Ptr { l: Option<LayoutId>, d: i64 },
    /// A known global's address: the constant `src`.
    Global { g: Global, src: u64 },
    /// The frame: the entry stack pointer plus `d`, or somewhere in the
    /// frame when `d` is `None`.
    Stack(Option<i64>),
    /// Conflicting facts: may be an address that needs moving.
    Top,
}

fn norm(o: &dyn LayoutOracle, l: Option<LayoutId>, d: i64) -> T {
    let d = match l {
        Some(l) if o.layout(l).src_size > 0 => d.rem_euclid(o.layout(l).src_size as i64),
        _ => d,
    };
    T::Ptr { l, d }
}

fn join(a: T, b: T) -> T {
    use T::*;
    match (a, b) {
        _ if a == b => a,
        (Null, p @ Ptr { .. }) | (p @ Ptr { .. }, Null) => p,
        (Null, Int) | (Int, Null) | (Const(_), Int) | (Int, Const(_)) | (Const(_), Const(_)) => Int,
        (Null, Const(_)) | (Const(_), Null) => Int,
        (Ptr { l: l1, .. }, Ptr { l: l2, .. }) if l1 == l2 => Top,
        (Stack(_), Stack(_)) => Stack(None),
        _ => Top,
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Ctx {
    regs: BTreeMap<GuestReg, T>,
    /// 8-byte stack slots by frame offset.
    slots: BTreeMap<i64, T>,
}

impl Ctx {
    fn reg(&self, r: GuestReg) -> T {
        self.regs.get(&r).copied().unwrap_or(T::Int)
    }
    fn join(&self, o: &Ctx) -> Ctx {
        let mut regs = BTreeMap::new();
        for (r, a) in &self.regs {
            regs.insert(*r, join(*a, o.reg(*r)));
        }
        for (r, b) in &o.regs {
            regs.entry(*r).or_insert(join(T::Int, *b));
        }
        regs.retain(|_, t| *t != T::Int);
        let mut slots = BTreeMap::new();
        for (k, a) in &self.slots {
            if let Some(b) = o.slots.get(k) {
                slots.insert(*k, join(*a, *b));
            }
        }
        Ctx { regs, slots }
    }
}

// ---------------------------------------------------------------------------
// Typing
// ---------------------------------------------------------------------------

struct Typing<'a> {
    o: &'a dyn LayoutOracle,
    abi: &'a Abi,
    vals: Vec<Option<T>>,
}

impl Typing<'_> {
    fn ty(&self, v: Value) -> T {
        self.vals
            .get(v.0 as usize)
            .copied()
            .flatten()
            .unwrap_or(T::Int)
    }

    fn constant(&self, c: u64) -> T {
        if c == 0 {
            return T::Null;
        }
        match self.o.global(c) {
            Some(g) => T::Global { g, src: c },
            None => T::Const(c),
        }
    }

    /// A global's address seen as a pointer into its layout.
    fn as_ptr(&self, t: T) -> T {
        match t {
            T::Global { g, src } => norm(self.o, Some(g.layout), (src - g.src) as i64),
            t => t,
        }
    }

    fn add(&self, a: T, b: T) -> T {
        use T::*;
        match (self.as_ptr(a), self.as_ptr(b)) {
            (Null, x) | (x, Null) => x,
            (Ptr { l, d }, Const(c)) | (Const(c), Ptr { l, d }) => norm(self.o, l, d + c as i64),
            (Ptr { l, d }, Int) | (Int, Ptr { l, d }) => T::Ptr { l, d },
            (Stack(Some(d)), Const(c)) | (Const(c), Stack(Some(d))) => Stack(Some(d + c as i64)),
            (Stack(_), Int)
            | (Int, Stack(_))
            | (Stack(None), Const(_))
            | (Const(_), Stack(None)) => Stack(None),
            (Const(x), Const(y)) => self.constant(x.wrapping_add(y)),
            (Int, Int) | (Int, Const(_)) | (Const(_), Int) => Int,
            _ => Top,
        }
    }

    fn sub(&self, a: T, b: T) -> T {
        use T::*;
        match (self.as_ptr(a), self.as_ptr(b)) {
            (x, Null) => x,
            (Ptr { l, d }, Const(c)) => norm(self.o, l, d - c as i64),
            (Stack(Some(d)), Const(c)) => Stack(Some(d - c as i64)),
            (Stack(_), Const(_) | Int) => Stack(None),
            (Ptr { .. }, Ptr { .. }) | (Stack(_), Stack(_)) => Int,
            (Const(x), Const(y)) => self.constant(x.wrapping_sub(y)),
            (Int | Const(_) | Null, Int | Const(_)) => Int,
            _ => Top,
        }
    }

    fn expr(&self, e: &Expr, ctx: &Ctx) -> T {
        match *e {
            Expr::ConstI64(c) => self.constant(c),
            Expr::ConstI32(c) => self.constant(c as u64),
            Expr::ReadReg(r) => ctx.reg(r),
            Expr::Bin {
                op: BinOp::Add,
                ty: Ty::I64,
                a,
                b,
            } => self.add(self.ty(a), self.ty(b)),
            Expr::Bin {
                op: BinOp::Sub,
                ty: Ty::I64,
                a,
                b,
            } => self.sub(self.ty(a), self.ty(b)),
            Expr::Extend { a, signed: false } | Expr::Wrap(a) => match self.ty(a) {
                t @ (T::Const(_) | T::Null | T::Global { .. }) => t,
                T::Ptr { .. } | T::Stack(_) | T::Top => T::Top,
                T::Int => T::Int,
            },
            Expr::Select { a, b, .. } => join(self.ty(a), self.ty(b)),
            Expr::Load {
                addr, offset, sz, ..
            } => self.load(addr, offset, sz, ctx).unwrap_or(T::Int),
            _ => T::Int,
        }
    }

    /// What a load produces, when it is typed; `Err` when it should be and
    /// is not.
    fn load(&self, addr: Value, offset: i64, sz: MemSz, ctx: &Ctx) -> Result<T, String> {
        match self.as_ptr(self.ty(addr)) {
            T::Ptr { l: Some(l), d } => access(self.o, l, d + offset, sz.bytes()).map(|a| a.result),
            T::Stack(Some(d)) if sz == MemSz::B8 => {
                Ok(ctx.slots.get(&(d + offset)).copied().unwrap_or(T::Int))
            }
            T::Stack(_) => Ok(T::Int),
            T::Ptr { l: None, .. } => Err("access through a pointer to an unknown type".into()),
            T::Top => Err("access through a value that may be a pointer".into()),
            _ => Ok(T::Int),
        }
    }

    /// Run `b`'s statements over `ctx`, typing every value it defines.
    fn block(&mut self, f: &Function, b: BlockId, ctx: &mut Ctx) {
        let blk = f.block(b);
        for s in &blk.stmts {
            match s {
                Stmt::Def { dst, expr } => {
                    let t = self.expr(expr, ctx);
                    self.vals[dst.0 as usize] = Some(t);
                }
                Stmt::WriteReg { reg, val } => {
                    let t = self.ty(*val);
                    if t == T::Int {
                        ctx.regs.remove(reg);
                    } else {
                        ctx.regs.insert(*reg, t);
                    }
                }
                Stmt::Store {
                    addr,
                    offset,
                    val,
                    sz,
                    ..
                } => {
                    if let T::Stack(at) = self.ty(*addr) {
                        match at {
                            Some(d) => {
                                let d = d + offset;
                                ctx.slots
                                    .retain(|k, _| *k + 8 <= d || *k >= d + sz.bytes() as i64);
                                if *sz == MemSz::B8 {
                                    ctx.slots.insert(d, self.ty(*val));
                                }
                            }
                            None => ctx.slots.clear(),
                        }
                    }
                }
                _ => {
                    for_each_def(s, |v| self.vals[v.0 as usize] = Some(T::Int));
                }
            }
        }
    }

    /// The context after `b`'s terminator, on its way to `to`.
    fn across(&self, f: &Function, b: BlockId, ctx: &Ctx) -> Ctx {
        let mut out = ctx.clone();
        match &f.block(b).term {
            FTerm::Call { callee, .. } => {
                out.regs
                    .retain(|r, _| self.abi.callee_saved.contains(*r) || *r == self.abi.sp);
                if let Some(T::Stack(Some(d))) = out.regs.get(&self.abi.sp).copied() {
                    out.regs
                        .insert(self.abi.sp, T::Stack(Some(d + self.abi.call_sp_delta)));
                    // The callee may write below the caller's stack pointer.
                    out.slots.retain(|k, _| *k >= d);
                } else {
                    out.regs.remove(&self.abi.sp);
                    out.slots.clear();
                }
                let ret = match callee {
                    Callee::Direct(a) => self.o.returns(*a),
                    Callee::Indirect(_) => None,
                };
                if let Some(Shape::Ptr(l)) = ret {
                    out.regs.insert(self.abi.ret, T::Ptr { l, d: 0 });
                }
            }
            FTerm::Syscall { .. } => {
                out.regs.remove(&self.abi.ret);
            }
            _ => {}
        }
        out
    }
}

/// Type every value of `f` to a fixpoint. Returns the typing and each
/// block's entry context.
fn type_function<'a>(
    f: &Function,
    o: &'a dyn LayoutOracle,
    abi: &'a Abi,
) -> (Typing<'a>, Vec<Option<Ctx>>) {
    let mut t = Typing {
        o,
        abi,
        vals: vec![None; f.n_values as usize],
    };
    let mut entry = Ctx {
        regs: BTreeMap::new(),
        slots: BTreeMap::new(),
    };
    entry.regs.insert(abi.sp, T::Stack(Some(0)));
    for (r, s) in o.args(f.entry) {
        if let Shape::Ptr(l) = s {
            entry.regs.insert(r, T::Ptr { l, d: 0 });
        }
    }
    let n = f.blocks.len();
    let mut ins: Vec<Option<Ctx>> = vec![None; n];
    let mut param_in: Vec<Vec<Option<T>>> = f
        .blocks
        .iter()
        .map(|b| vec![None; b.params.len()])
        .collect();
    for b in f.entry_blocks() {
        // Interior entries come in with nothing known.
        let c = if b == BlockId(0) {
            entry.clone()
        } else {
            Ctx {
                regs: BTreeMap::new(),
                slots: BTreeMap::new(),
            }
        };
        for (i, p) in f.block(b).params.iter().enumerate() {
            param_in[b.index()][i] = Some(c.reg(p.reg));
        }
        ins[b.index()] = Some(c);
    }
    let mut work: Vec<BlockId> = f.rpo();
    work.reverse();
    let mut rounds = 0;
    while let Some(b) = work.pop() {
        rounds += 1;
        if rounds > 64 * n + 64 {
            break;
        }
        let Some(mut ctx) = ins[b.index()].clone() else {
            continue;
        };
        for (i, p) in f.block(b).params.iter().enumerate() {
            t.vals[p.val.0 as usize] = Some(param_in[b.index()][i].unwrap_or(T::Int));
        }
        t.block(f, b, &mut ctx);
        let out = t.across(f, b, &ctx);
        for e in f.block(b).term.edges() {
            let to = e.to.index();
            let mut changed = false;
            let new = match &ins[to] {
                None => out.clone(),
                Some(c) => c.join(&out),
            };
            if ins[to].as_ref() != Some(&new) {
                ins[to] = Some(new);
                changed = true;
            }
            for (i, a) in e.args.iter().enumerate() {
                let at = t.ty(*a);
                let j = match param_in[to][i] {
                    None => at,
                    Some(p) => join(p, at),
                };
                if param_in[to][i] != Some(j) {
                    param_in[to][i] = Some(j);
                    changed = true;
                }
            }
            if changed && !work.contains(&e.to) {
                work.push(e.to);
            }
        }
    }
    // A last sweep in order so every value carries its final type.
    for b in f.rpo() {
        let Some(mut ctx) = ins[b.index()].clone() else {
            continue;
        };
        for (i, p) in f.block(b).params.iter().enumerate() {
            t.vals[p.val.0 as usize] = Some(param_in[b.index()][i].unwrap_or(T::Int));
        }
        t.block(f, b, &mut ctx);
    }
    (t, ins)
}

// ---------------------------------------------------------------------------
// Rewriting
// ---------------------------------------------------------------------------

fn elem_sizes(o: &dyn LayoutOracle, l: LayoutId) -> (i64, i64) {
    let lay = o.layout(l);
    (lay.src_size as i64, lay.dst_size as i64)
}

/// Statements computing `x` (a byte distance in units of `from`-sized
/// elements) in units of `to`-sized ones: `x / from * to`, a shift when the
/// ratio is a power of two. Returns the new value.
fn rescale(
    f: &mut Function,
    out: &mut Vec<(Stmt, u64)>,
    pc: u64,
    x: Value,
    from: i64,
    to: i64,
) -> Value {
    let def = |f: &mut Function, out: &mut Vec<(Stmt, u64)>, expr: Expr| {
        let dst = f.fresh();
        out.push((Stmt::Def { dst, expr }, pc));
        dst
    };
    if from > to && from % to == 0 && ((from / to) as u64).is_power_of_two() {
        let k = def(f, out, Expr::ConstI64((from / to).trailing_zeros() as u64));
        def(
            f,
            out,
            Expr::Bin {
                op: BinOp::ShrS,
                ty: Ty::I64,
                a: x,
                b: k,
            },
        )
    } else if to > from && to % from == 0 && ((to / from) as u64).is_power_of_two() {
        let k = def(f, out, Expr::ConstI64((to / from).trailing_zeros() as u64));
        def(
            f,
            out,
            Expr::Bin {
                op: BinOp::Shl,
                ty: Ty::I64,
                a: x,
                b: k,
            },
        )
    } else {
        let a = def(f, out, Expr::ConstI64(from as u64));
        let q = def(
            f,
            out,
            Expr::Bin {
                op: BinOp::DivS,
                ty: Ty::I64,
                a: x,
                b: a,
            },
        );
        let b = def(f, out, Expr::ConstI64(to as u64));
        def(
            f,
            out,
            Expr::Bin {
                op: BinOp::Mul,
                ty: Ty::I64,
                a: q,
                b,
            },
        )
    }
}

/// Relay out `f` (see the module documentation).
pub fn relayout(f: &mut Function, o: &dyn LayoutOracle, abi: &Abi) -> Report {
    let (t, ins) = type_function(f, o, abi);
    let mut rep = Report::default();
    for b in 0..f.blocks.len() {
        let Some(mut ctx) = ins[b].clone() else {
            continue;
        };
        let old = std::mem::take(&mut f.blocks[b].stmts);
        let pcs = std::mem::take(&mut f.blocks[b].pcs);
        let mut out: Vec<(Stmt, u64)> = Vec::with_capacity(old.len());
        // `ctx` is advanced statement by statement for stack-slot loads.
        let mut tt = Typing {
            o,
            abi,
            vals: t.vals.clone(),
        };
        for (mut s, pc) in old.into_iter().zip(pcs) {
            // A pointer difference renamed to `tmp`, to be rescaled into `orig`.
            let mut diff: Option<(Value, Value, i64, i64)> = None;
            match &mut s {
                Stmt::Def { dst, expr } => match expr {
                    Expr::ConstI64(_) | Expr::ConstI32(_)
                        if matches!(t.ty(*dst), T::Global { .. }) =>
                    {
                        let src = match expr {
                            Expr::ConstI64(c) => *c,
                            Expr::ConstI32(c) => *c as u64,
                            _ => unreachable!(),
                        };
                        let T::Global { g, .. } = t.ty(*dst) else {
                            unreachable!()
                        };
                        match map_off(o, g.layout, (src - g.src) as i64) {
                            Some(d) => {
                                let n = (g.dst as i64 + d) as u64;
                                *expr = match expr {
                                    Expr::ConstI64(_) => Expr::ConstI64(n),
                                    _ => Expr::ConstI32(n as u32),
                                };
                                rep.globals += 1;
                            }
                            None => rep.untyped.push((
                                pc,
                                format!("address {src:#x} inside a global, between fields"),
                            )),
                        }
                    }
                    Expr::Load {
                        addr,
                        offset,
                        sz,
                        signed,
                        ..
                    } => match t.as_ptr(t.ty(*addr)) {
                        T::Ptr { l: Some(l), d } => match access(o, l, d + *offset, sz.bytes()) {
                            Ok(a) => {
                                let base = map_off(o, l, d).unwrap_or(0);
                                let new_off = a.dst - base;
                                if new_off != *offset || a.sz != *sz {
                                    rep.accesses += 1;
                                } else {
                                    rep.unchanged += 1;
                                }
                                if map_off(o, l, d).is_none() {
                                    rep.untyped.push((
                                        pc,
                                        format!(
                                            "base {d} into {} is between fields",
                                            o.layout(l).name
                                        ),
                                    ));
                                }
                                *offset = new_off;
                                if a.sz != *sz {
                                    *sz = a.sz;
                                    *signed = a.signed.unwrap_or(false);
                                }
                            }
                            Err(e) => rep.untyped.push((pc, e)),
                        },
                        T::Ptr { l: None, .. } => rep
                            .untyped
                            .push((pc, "load through a pointer to an unknown type".into())),
                        T::Top => rep
                            .untyped
                            .push((pc, "load through a value that may be a pointer".into())),
                        _ => {}
                    },
                    Expr::Bin {
                        op: op @ (BinOp::Add | BinOp::Sub),
                        ty: Ty::I64,
                        a,
                        b,
                    } => {
                        let (ta, tb) = (t.as_ptr(t.ty(*a)), t.as_ptr(t.ty(*b)));
                        let is_sub = *op == BinOp::Sub;
                        // Pointer and step, either order for an add.
                        let pick = match (ta, tb) {
                            (T::Ptr { l, d }, x) if !matches!(x, T::Ptr { .. } | T::Null) => {
                                Some((l, d, false, x))
                            }
                            (x, T::Ptr { l, d })
                                if !is_sub && !matches!(x, T::Ptr { .. } | T::Null) =>
                            {
                                Some((l, d, true, x))
                            }
                            _ => None,
                        };
                        if let (T::Ptr { l: l1, .. }, T::Ptr { l: l2, .. }) = (ta, tb) {
                            if is_sub {
                                // A pointer difference, back to source units.
                                match (l1, l2) {
                                    (Some(x), Some(y)) if x == y => {
                                        let (ss, ds) = elem_sizes(o, x);
                                        if ss != ds {
                                            let tmp = f.fresh();
                                            diff = Some((*dst, tmp, ds, ss));
                                            *dst = tmp;
                                        }
                                    }
                                    _ => rep.untyped.push((
                                        pc,
                                        "difference of pointers to different or unknown types"
                                            .into(),
                                    )),
                                }
                            }
                        } else if let Some((l, d, swapped, step)) = pick {
                            let stepv = if swapped { *a } else { *b };
                            match (l, step) {
                                (Some(l), T::Const(c)) => {
                                    let c = if is_sub {
                                        (c as i64).wrapping_neg()
                                    } else {
                                        c as i64
                                    };
                                    match (map_off(o, l, d), map_off(o, l, d + c)) {
                                        (Some(x), Some(y)) if y - x != c => {
                                            let k = f.fresh();
                                            let v = if is_sub {
                                                (x - y) as u64
                                            } else {
                                                (y - x) as u64
                                            };
                                            out.push((
                                                Stmt::Def {
                                                    dst: k,
                                                    expr: Expr::ConstI64(v),
                                                },
                                                pc,
                                            ));
                                            if swapped {
                                                *a = k;
                                            } else {
                                                *b = k;
                                            }
                                            rep.arithmetic += 1;
                                        }
                                        (Some(_), Some(_)) => {}
                                        _ => rep.untyped.push((
                                            pc,
                                            format!(
                                                "step {c} from {d} in {} lands between fields",
                                                o.layout(l).name
                                            ),
                                        )),
                                    }
                                }
                                (Some(l), T::Int) => {
                                    let (ss, ds) = elem_sizes(o, l);
                                    if ss != ds {
                                        let mut extra = Vec::new();
                                        let k = rescale(f, &mut extra, pc, stepv, ss, ds);
                                        out.extend(extra);
                                        if swapped {
                                            *a = k;
                                        } else {
                                            *b = k;
                                        }
                                        rep.arithmetic += 1;
                                    }
                                }
                                (None, T::Const(_) | T::Int) => rep.untyped.push((
                                    pc,
                                    "arithmetic on a pointer to an unknown type".into(),
                                )),
                                _ => {}
                            }
                        }
                    }
                    Expr::VLoad { addr, .. }
                    | Expr::Rmw { addr, .. }
                    | Expr::CmpXchg { addr, .. } => {
                        if matches!(t.as_ptr(t.ty(*addr)), T::Ptr { .. } | T::Top) {
                            rep.untyped.push((
                                pc,
                                "vector or atomic access through a typed pointer".into(),
                            ));
                        }
                    }
                    _ => {}
                },
                Stmt::Store {
                    addr, offset, sz, ..
                } => match t.as_ptr(t.ty(*addr)) {
                    T::Ptr { l: Some(l), d } => match access(o, l, d + *offset, sz.bytes()) {
                        Ok(a) => {
                            let base = map_off(o, l, d).unwrap_or(0);
                            let new_off = a.dst - base;
                            if new_off != *offset || a.sz != *sz {
                                rep.accesses += 1;
                            } else {
                                rep.unchanged += 1;
                            }
                            *offset = new_off;
                            *sz = a.sz;
                        }
                        Err(e) => rep.untyped.push((pc, e)),
                    },
                    T::Ptr { l: None, .. } => rep
                        .untyped
                        .push((pc, "store through a pointer to an unknown type".into())),
                    T::Top => rep
                        .untyped
                        .push((pc, "store through a value that may be a pointer".into())),
                    _ => {}
                },
                Stmt::VStore { addr, .. } => {
                    if matches!(t.as_ptr(t.ty(*addr)), T::Ptr { .. } | T::Top) {
                        rep.untyped
                            .push((pc, "vector or atomic access through a typed pointer".into()));
                    }
                }
                Stmt::MemCopy { dst, src, .. } => {
                    if [*dst, *src]
                        .iter()
                        .any(|v| matches!(t.as_ptr(t.ty(*v)), T::Ptr { .. } | T::Top))
                    {
                        rep.untyped
                            .push((pc, "block copy through a typed pointer".into()));
                    }
                }
                Stmt::MemFill { dst, .. } => {
                    if matches!(t.as_ptr(t.ty(*dst)), T::Ptr { .. } | T::Top) {
                        rep.untyped
                            .push((pc, "block fill through a typed pointer".into()));
                    }
                }
                _ => {}
            }
            tt.block_stmt(&s, &mut ctx);
            out.push((s, pc));
            if let Some((orig, tmp, from, to)) = diff {
                let mut extra = Vec::new();
                rescale(f, &mut extra, pc, tmp, from, to);
                if let Some((Stmt::Def { dst, .. }, _)) = extra.last_mut() {
                    *dst = orig;
                }
                out.extend(extra);
                rep.arithmetic += 1;
            }
        }
        // A frame address leaving in an argument register: the callee sees
        // the frame's layout, which this pass does not change.
        if let FTerm::Call { .. } = f.blocks[b].term {
            for r in abi.arg_regs {
                if matches!(ctx.reg(*r), T::Stack(_)) {
                    rep.untyped.push((
                        f.blocks[b].term_pc,
                        format!("frame address passed in {r:?}"),
                    ));
                }
            }
        }
        f.blocks[b].pcs = out.iter().map(|(_, p)| *p).collect();
        f.blocks[b].stmts = out.into_iter().map(|(s, _)| s).collect();
    }
    rep
}

impl Typing<'_> {
    /// [`Typing::block`] for one statement, without retyping values.
    fn block_stmt(&mut self, s: &Stmt, ctx: &mut Ctx) {
        match s {
            Stmt::WriteReg { reg, val } => {
                let t = self.ty(*val);
                if t == T::Int {
                    ctx.regs.remove(reg);
                } else {
                    ctx.regs.insert(*reg, t);
                }
            }
            Stmt::Store {
                addr,
                offset,
                val,
                sz,
                ..
            } => {
                if let T::Stack(at) = self.ty(*addr) {
                    match at {
                        Some(d) => {
                            let d = d + offset;
                            ctx.slots
                                .retain(|k, _| *k + 8 <= d || *k >= d + sz.bytes() as i64);
                            if *sz == MemSz::B8 {
                                ctx.slots.insert(d, self.ty(*val));
                            }
                        }
                        None => ctx.slots.clear(),
                    }
                }
            }
            _ => {}
        }
    }
}
