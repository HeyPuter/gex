//! Tier-1 code generation: one wasm function per guest basic block.
//!
//! The shape is PLAN §2.4: every block is a function with the uniform
//! signature `(ctx: i32) -> ()`; direct successors are reached with
//! `return_call`, computed ones with `return_call_indirect` through the
//! PC-indexed dispatch table. Guest stack depth therefore never shows up as
//! wasm stack depth, and any 4-byte-aligned address can become an entry point
//! later without retranslating anything.
//!
//! Two things make tier-1 more than a naive transliteration:
//!
//! * **Guest registers are cached in wasm locals** for the length of the
//!   block. A register is loaded from the context at most once and stored
//!   back at most once, at the block's exit (PLAN §5.4.2).
//! * **Value locals are recycled** using last-use information, so a long
//!   block needs a handful of locals rather than one per SSA value.

use crate::wasm::{FuncBody, ValType};
use gex_abi::{ctx, DISPATCH_DIR, DISPATCH_MAX_PAGES, DISPATCH_PAGE, GUEST_LIMIT, POISON_ADDR};
use gex_ir::*;
use std::collections::HashMap;

/// SIMD sub-opcodes (the 0xFD prefix space).
mod simd {
    pub const V128_LOAD: u32 = 0x00;
    pub const V128_STORE: u32 = 0x0b;
    pub const I8X16_EXTRACT_LANE_S: u32 = 0x15;
    pub const I8X16_EXTRACT_LANE_U: u32 = 0x16;
    pub const I8X16_REPLACE_LANE: u32 = 0x17;
    pub const I16X8_EXTRACT_LANE_S: u32 = 0x18;
    pub const I16X8_EXTRACT_LANE_U: u32 = 0x19;
    pub const I16X8_REPLACE_LANE: u32 = 0x1a;
    pub const I32X4_EXTRACT_LANE: u32 = 0x1b;
    pub const I32X4_REPLACE_LANE: u32 = 0x1c;
    pub const I64X2_EXTRACT_LANE: u32 = 0x1d;
    pub const I64X2_REPLACE_LANE: u32 = 0x1e;
    pub const I8X16_SPLAT: u32 = 0x0f;
    pub const I16X8_SPLAT: u32 = 0x10;
    pub const I32X4_SPLAT: u32 = 0x11;
    pub const I64X2_SPLAT: u32 = 0x12;
    pub const I8X16_SWIZZLE: u32 = 0x0e;
    pub const V128_BITSELECT: u32 = 0x52;
    pub const I8X16_POPCNT: u32 = 0x62;
    pub const V128_AND: u32 = 0x4e;
    pub const V128_ANDNOT: u32 = 0x4f;
    pub const V128_OR: u32 = 0x50;
    pub const V128_XOR: u32 = 0x51;
    // Only the narrow lane loads are used: 4- and 8-byte loads have
    // dedicated zeroing forms, which are one instruction instead of two.
    pub const V128_LOAD8_LANE: u32 = 0x54;
    pub const V128_LOAD16_LANE: u32 = 0x55;
    pub const V128_STORE8_LANE: u32 = 0x58;
    pub const V128_STORE16_LANE: u32 = 0x59;
    pub const V128_STORE32_LANE: u32 = 0x5a;
    pub const V128_STORE64_LANE: u32 = 0x5b;
    pub const V128_LOAD32_ZERO: u32 = 0x5c;
    pub const V128_LOAD64_ZERO: u32 = 0x5d;
}

/// The wasm value type backing a GIR type.
fn val_type(ty: Ty) -> ValType {
    match ty {
        Ty::I32 => ValType::I32,
        Ty::I64 => ValType::I64,
        Ty::V128 => ValType::V128,
    }
}

// Numeric opcodes, grouped so the i32/i64 pairs stay visibly adjacent.
mod code {
    pub const I32_WRAP_I64: u8 = 0xa7;
    pub const I64_EXTEND_I32_S: u8 = 0xac;
    pub const I64_EXTEND_I32_U: u8 = 0xad;
    pub const I32_EXTEND8_S: u8 = 0xc0;
    pub const I32_EXTEND16_S: u8 = 0xc1;
    pub const I64_EXTEND8_S: u8 = 0xc2;
    pub const I64_EXTEND16_S: u8 = 0xc3;
    pub const I64_EXTEND32_S: u8 = 0xc4;
    pub const I64_LT_U: u8 = 0x54;
}

/// Everything tier-1 needs to know about the module it is emitting into.
pub struct Context<'a> {
    /// Import index of `gex.syscall`.
    pub syscall: u32,
    /// Import index of `gex.illegal`.
    pub illegal: u32,
    /// Import index of `gex.breakpoint`.
    pub breakpoint: u32,
    /// Import index of `gex.sysreg_read`.
    pub sysreg_read: u32,
    /// Import index of `gex.fma`.
    pub fma: u32,
    /// Import index of `gex.sysreg_write`.
    pub sysreg_write: u32,
    /// Import index of `gex.cache_op`.
    pub cache_op: u32,
    /// Import index of `gex.dispatch_miss`.
    pub dispatch_miss: u32,
    /// Import index of `gex.safepoint`.
    pub safepoint: u32,
    /// Type index of the uniform block signature `(i32) -> ()`.
    pub block_ty: u32,
    /// Guest address -> wasm function index, for blocks already assigned an
    /// index. A miss falls back to the dispatch table.
    pub blocks: &'a HashMap<u64, u32>,
    /// Guest address -> the block's GIR, for superblock formation.
    ///
    /// A block whose only successor is a statically known one can have that
    /// successor's body emitted in line instead of tail-calling it. The tail
    /// call is the expensive part -- an indirect one costs about fifty cycles
    /// under wasmtime -- and folding also lets the register cache live across
    /// the join instead of flushing at it.
    pub gir: &'a HashMap<u64, &'a Block>,
    /// How many blocks in this unit branch to each address.
    ///
    /// Folding is restricted to targets with exactly one predecessor, so no
    /// code is ever duplicated. Duplicating it -- folding into every
    /// predecessor -- was measurably worse: 10-18% more wasm, and Lua and
    /// matmul both ran slower for it, which outweighed the tail calls saved.
    pub preds: &'a HashMap<u64, u32>,
}

/// How many blocks may be folded into one function.
fn max_chain() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("GEX_MAX_CHAIN")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8)
    })
}

/// Compile one GIR block into a wasm function body.
pub fn compile(block: &Block, cx: &Context<'_>) -> Vec<u8> {
    let mut e = Emitter::new(cx);
    e.run(block);
    e.f.finish()
}

/// Whether `block`'s unconditional successor can be folded into it.
///
/// Only a plain `Goto`, only forwards, and only to a block this module has the
/// GIR for. Forwards-only is what keeps a chain finite without tracking what
/// is already in it: a back-edge is a loop, and loop fusion handles the one
/// shape of that worth handling.
fn chain_target(block: &Block, cx: &Context<'_>) -> Option<u64> {
    // A kill switch, so the transform can be measured against its absence.
    if std::env::var_os("GEX_NO_SUPERBLOCK").is_some() {
        return None;
    }
    match block.term {
        Term::Goto { target }
            if target > block.start
                && cx.gir.contains_key(&target)
                && cx.preds.get(&target) == Some(&1) =>
        {
            Some(target)
        }
        _ => None,
    }
}

/// Where a guest register currently lives.
struct Cached {
    local: u32,
    dirty: bool,
}

struct Emitter<'a> {
    f: FuncBody,
    cx: &'a Context<'a>,
    /// The `ctx` parameter.
    ctx_local: u32,
    /// Scratch for address computation.
    scratch_i64: u32,
    /// Scratch for the safepoint counter.
    scratch_i32: u32,
    /// Scratch for the dispatch page index.
    scratch_page: u32,
    /// Locals holding live GIR values.
    value_local: Vec<Option<u32>>,
    /// Constant value of each GIR value, when known.
    value_const: Vec<Option<u64>>,
    /// Type of each GIR value, needed to pick the right local pool.
    value_ty: Vec<Ty>,
    /// Free locals available for reuse, by type.
    free_i32: Vec<u32>,
    free_i64: Vec<u32>,
    free_v128: Vec<u32>,
    /// Guest register cache.
    regs: HashMap<GuestReg, Cached>,
    /// Set while emitting a block fused into a `wasm` loop: how many nested
    /// `if`s sit between here and the loop label, so a back-edge knows what
    /// depth to branch to. `None` when the block is not fused.
    loop_depth: Option<u32>,
    /// Counts down to the next re-entry through the dispatcher, inside a
    /// fused loop. See `emit_reentry`.
    reenter_local: Option<u32>,
}

impl<'a> Emitter<'a> {
    fn new(cx: &'a Context<'a>) -> Emitter<'a> {
        let mut f = FuncBody::new(1);
        let scratch_i64 = f.add_local(ValType::I64);
        let scratch_i32 = f.add_local(ValType::I32);
        let scratch_page = f.add_local(ValType::I64);
        Emitter {
            f,
            cx,
            ctx_local: 0,
            scratch_i64,
            scratch_i32,
            scratch_page,
            value_local: Vec::new(),
            value_const: Vec::new(),
            value_ty: Vec::new(),
            free_i32: Vec::new(),
            free_i64: Vec::new(),
            free_v128: Vec::new(),
            regs: HashMap::new(),
            loop_depth: None,
            reenter_local: None,
        }
    }

    fn run(&mut self, block: &Block) {
        let mut current = block;
        for i in 0..max_chain() {
            // Whether this block's terminator is replaced by the next block's
            // body has to be decided *before* emitting it: deciding after
            // would leave the last block of a chain that hit the limit with a
            // terminator and no body.
            let next = if i + 1 == max_chain() {
                None
            } else {
                chain_target(current, self.cx)
            };
            // A block folded into this one keeps the register cache, which is
            // most of the saving: no flush at the join, no reload after it.
            self.run_one(current, next.is_some());
            match next {
                Some(a) => current = self.cx.gir[&a],
                None => return,
            }
        }
    }

    fn run_one(&mut self, block: &Block, chained: bool) {
        let n = block.n_values as usize;
        self.value_local = vec![None; n];
        self.value_const = vec![None; n];
        self.value_ty = vec![Ty::I64; n];

        let last_use = compute_last_use(block);

        // A block whose only successor is itself becomes a real wasm loop, so
        // the guest registers it carries stay in locals across the back-edge
        // instead of round-tripping through the context every iteration. That
        // is only sound if every register the body reads is loaded *before*
        // the loop -- otherwise a second iteration would read a context that
        // the first iteration deliberately did not write back.
        if self.can_fuse_self_loop(block) {
            let mut reads = live_in_registers(block);
            reads.sort_by_key(|r| Self::reg_offset(*r));
            for r in reads {
                self.reg_local(r);
            }
            let counter = self.alloc(Ty::I32);
            self.f.i32_const(loop_poll());
            self.f.local_set(counter);
            self.reenter_local = Some(counter);
            self.f.loop_void();
            self.loop_depth = Some(0);
        }

        for (i, stmt) in block.stmts.iter().enumerate() {
            self.stmt(stmt, block);
            // Recycle locals whose values are dead after this statement.
            for (v, &lu) in last_use.iter().enumerate() {
                if lu == Some(i) {
                    self.release(Value(v as u32));
                }
            }
        }
        // A chained block's terminator is the next block's body.
        if !chained {
            self.terminator(block);
        }
        if self.loop_depth.is_some() {
            self.f.end();
            self.loop_depth = None;
            self.reenter_local = None;
        }
    }

    /// Whether this block can be emitted as a self-contained wasm loop.
    ///
    /// The body must not contain anything that invalidates the register cache
    /// -- a host call can rewrite guest state, and after one the cached locals
    /// no longer describe the machine.
    fn can_fuse_self_loop(&self, block: &Block) -> bool {
        let back_edge = match block.term {
            Term::Goto { target } => target == block.start,
            Term::Branch {
                taken, not_taken, ..
            } => taken == block.start || not_taken == block.start,
            _ => false,
        };
        // A single-block loop that is also its own only successor would spin
        // forever with no exit; `Goto` to itself is exactly that, but it still
        // needs the safepoint, which the fused form emits.
        // A kill switch, so the fused and unfused forms can be compared
        // directly and a suspected miscompilation can be bisected.
        if std::env::var_os("GEX_NO_LOOP_FUSION").is_some() {
            return false;
        }
        back_edge && !block.stmts.iter().any(clobbers_guest_state)
    }

    // -- local management --------------------------------------------------

    fn alloc(&mut self, ty: Ty) -> u32 {
        let pool = match ty {
            Ty::I32 => &mut self.free_i32,
            Ty::I64 => &mut self.free_i64,
            Ty::V128 => &mut self.free_v128,
        };
        if let Some(l) = pool.pop() {
            return l;
        }
        self.f.add_local(val_type(ty))
    }

    fn release(&mut self, v: Value) {
        let idx = v.0 as usize;
        if idx >= self.value_local.len() {
            return;
        }
        if let Some(local) = self.value_local[idx].take() {
            match self.value_ty[idx] {
                Ty::I32 => self.free_i32.push(local),
                Ty::I64 => self.free_i64.push(local),
                Ty::V128 => self.free_v128.push(local),
            }
        }
    }

    /// Push a value onto the wasm operand stack.
    fn push(&mut self, v: Value) {
        let idx = v.0 as usize;
        match self.value_local.get(idx).copied().flatten() {
            Some(l) => self.f.local_get(l),
            None => {
                // A value with no local is either a folded constant or a bug
                // in liveness; constants are re-materialised inline, which is
                // cheaper than a local anyway.
                match self.value_const.get(idx).copied().flatten() {
                    Some(c) if self.value_ty[idx] == Ty::I32 => self.f.i32_const(c as i32),
                    Some(c) => self.f.i64_const(c as i64),
                    None => panic!("value {v:?} used after release"),
                }
            }
        }
    }

    // -- guest register cache ---------------------------------------------

    fn reg_offset(r: GuestReg) -> u32 {
        (match r {
            GuestReg::X(n) => ctx::x(n),
            GuestReg::V128(n) => ctx::v(n),
            GuestReg::Sp => ctx::SP,
            GuestReg::Pc => ctx::PC,
            GuestReg::N => ctx::NF,
            GuestReg::Z => ctx::ZF,
            GuestReg::C => ctx::CF,
            GuestReg::V => ctx::VF,
            GuestReg::Tpidr => ctx::TPIDR,
            GuestReg::TpidrRo => ctx::TPIDRRO,
            GuestReg::MonAddr => ctx::MON_ADDR,
            GuestReg::MonValue => ctx::MON_VALUE,
            GuestReg::MonSize => ctx::MON_SIZE,
        }) as u32
    }

    /// The local holding `r`, loading it from the context on first use.
    fn reg_local(&mut self, r: GuestReg) -> u32 {
        if let Some(c) = self.regs.get(&r) {
            return c.local;
        }
        let ty = r.ty();
        let local = self.f.add_local(val_type(ty));
        let before = self.f.code.len();
        self.f.local_get(self.ctx_local);
        match ty {
            Ty::I32 => self.f.load(0x28, 2, Self::reg_offset(r)),
            Ty::I64 => self.f.load(0x29, 3, Self::reg_offset(r)),
            Ty::V128 => self.f.simd_mem(simd::V128_LOAD, 4, Self::reg_offset(r)),
        }
        self.f.local_set(local);
        CTX_BYTES.with(|c| c.set(c.get() + (self.f.code.len() - before)));
        self.regs.insert(
            r,
            Cached {
                local,
                dirty: false,
            },
        );
        local
    }

    /// Mark `r` as holding the value currently on top of the stack.
    fn reg_store_from_stack(&mut self, r: GuestReg) {
        let ty = r.ty();
        let local = match self.regs.get(&r) {
            Some(c) => c.local,
            None => self.f.add_local(val_type(ty)),
        };
        self.f.local_set(local);
        self.regs.insert(r, Cached { local, dirty: true });
    }

    /// Write every dirty cached register back to the context.
    fn flush(&mut self) {
        self.flush_inner(false);
    }

    /// Flush, but leave the registers marked dirty.
    ///
    /// Used where the flush is emitted inside a conditional: the stores only
    /// happen on some executions, so the emitter must keep believing the
    /// context is stale and write them back again on the paths that leave.
    fn flush_conditional(&mut self) {
        self.flush_inner(true);
    }

    fn flush_inner(&mut self, keep_dirty: bool) {
        let mut dirty: Vec<(GuestReg, u32)> = self
            .regs
            .iter()
            .filter(|(_, c)| c.dirty)
            .map(|(r, c)| (*r, c.local))
            .collect();
        // Deterministic output makes translated modules reproducible, which
        // the content-addressed cache (PLAN §10) depends on.
        dirty.sort_by_key(|(r, _)| Self::reg_offset(*r));
        let before = self.f.code.len();
        for (r, local) in dirty {
            self.f.local_get(self.ctx_local);
            self.f.local_get(local);
            match r.ty() {
                Ty::I32 => self.f.store(0x36, 2, Self::reg_offset(r)),
                Ty::I64 => self.f.store(0x37, 3, Self::reg_offset(r)),
                Ty::V128 => self.f.simd_mem(simd::V128_STORE, 4, Self::reg_offset(r)),
            }
            if !keep_dirty {
                if let Some(c) = self.regs.get_mut(&r) {
                    c.dirty = false;
                }
            }
        }
        CTX_BYTES.with(|c| c.set(c.get() + (self.f.code.len() - before)));
    }

    /// Forget every cached register, forcing later reads to reload. Used
    /// after a host call that can rewrite guest state.
    fn invalidate(&mut self) {
        self.regs.clear();
    }

    /// Record the guest PC in the context, so a host call or dispatch miss
    /// sees a precise architectural state.
    fn store_pc(&mut self, pc: u64) {
        self.f.local_get(self.ctx_local);
        self.f.i64_const(pc as i64);
        self.f.store(0x37, 3, ctx::PC as u32);
    }

    // -- addressing --------------------------------------------------------

    /// Emit the 32-bit linear-memory address for `addr + offset`, returning
    /// the static offset the access should carry.
    ///
    /// Guest VA equals linear-memory offset (PLAN §2.3), so this is a
    /// truncation plus one guard: an effective address that does not fit in
    /// 32 bits is steered to an address inside the unmapped top page, which
    /// makes the access trap and become SIGSEGV rather than silently aliasing
    /// low memory.
    fn emit_address(&mut self, addr: Value, offset: i64) -> u32 {
        // Constant addresses are extremely common (literal pools, absolute
        // references) and need no guard at all.
        if let Some(c) = self.value_const.get(addr.0 as usize).copied().flatten() {
            let ea = c.wrapping_add(offset as u64);
            if ea < GUEST_LIMIT {
                self.f.i32_const(ea as i32);
                return 0;
            }
            self.f.i32_const(POISON_ADDR as i32);
            return 0;
        }

        let static_off = if (0..=u32::MAX as i64).contains(&offset) {
            self.push(addr);
            offset as u32
        } else {
            self.push(addr);
            self.f.i64_const(offset);
            self.f.byte(0x7c); // i64.add
            0
        };
        self.guard_to_i32();
        static_off
    }

    /// Consume an `i64` effective address and leave a bounds-safe `i32`.
    fn guard_to_i32(&mut self) {
        let t = self.scratch_i64;
        self.f.local_set(t);
        self.f.local_get(t);
        self.f.byte(code::I32_WRAP_I64);
        self.f.i32_const(POISON_ADDR as i32);
        self.f.local_get(t);
        self.f.i64_const(GUEST_LIMIT as i64);
        self.f.byte(code::I64_LT_U);
        self.f.select();
    }

    // -- statements --------------------------------------------------------

    fn stmt(&mut self, s: &Stmt, block: &Block) {
        match s {
            Stmt::Def { dst, expr } => self.def(*dst, expr),
            Stmt::WriteReg { reg, val } => {
                self.push(*val);
                self.reg_store_from_stack(*reg);
            }
            Stmt::Store {
                addr,
                offset,
                val,
                sz,
                order,
            } => {
                let static_off = self.emit_address(*addr, *offset);
                self.push(*val);
                let ty = self.type_of(*val);
                if *order == Order::SeqCst {
                    let sub = atomic_store_op(ty, *sz);
                    self.f.atomic(sub, natural_align(*sz), static_off);
                } else {
                    let opcode = store_op(ty, *sz);
                    self.f.store(opcode, natural_align(*sz), static_off);
                }
            }
            Stmt::HostCall { func, args } => {
                self.host_call(*func, args, None, block);
            }
            Stmt::VStore {
                addr,
                offset,
                val,
                bytes,
            } => {
                let static_off = self.emit_address(*addr, *offset);
                self.push(*val);
                let align = bytes.trailing_zeros();
                if *bytes == 16 {
                    self.f.simd_mem(simd::V128_STORE, align, static_off);
                } else {
                    let sub = match bytes {
                        1 => simd::V128_STORE8_LANE,
                        2 => simd::V128_STORE16_LANE,
                        4 => simd::V128_STORE32_LANE,
                        _ => simd::V128_STORE64_LANE,
                    };
                    self.f.simd_mem_lane(sub, align, static_off, 0);
                }
            }
            Stmt::Fence => self.f.atomic_fence(),
        }
    }

    fn def(&mut self, dst: Value, expr: &Expr) {
        let ty = expr.ty();
        let idx = dst.0 as usize;
        self.value_ty[idx] = ty;

        match expr {
            Expr::ConstI32(c) => {
                self.value_const[idx] = Some(*c as u64);
                // Constants are re-materialised at each use rather than
                // occupying a local; `push` handles that.
                return;
            }
            Expr::ConstI64(c) => {
                self.value_const[idx] = Some(*c);
                return;
            }
            Expr::ReadReg(r) => {
                let local = self.reg_local(*r);
                self.f.local_get(local);
            }
            Expr::Bin { op, ty, a, b } => {
                self.push(*a);
                self.push(*b);
                self.f.byte(bin_op(*op, *ty));
            }
            Expr::Un { op, ty, a } => match op {
                UnOp::Not => {
                    self.push(*a);
                    self.const_of(*ty, -1i64 as u64);
                    self.f.byte(bin_op(BinOp::Xor, *ty));
                }
                UnOp::Neg => {
                    self.const_of(*ty, 0);
                    self.push(*a);
                    self.f.byte(bin_op(BinOp::Sub, *ty));
                }
                UnOp::Clz => {
                    self.push(*a);
                    self.f.byte(if *ty == Ty::I32 { 0x67 } else { 0x79 });
                }
                UnOp::Ctz => {
                    self.push(*a);
                    self.f.byte(if *ty == Ty::I32 { 0x68 } else { 0x7a });
                }
                UnOp::Popcnt => {
                    self.push(*a);
                    self.f.byte(if *ty == Ty::I32 { 0x69 } else { 0x7b });
                }
            },
            Expr::Cmp { op, ty, a, b } => {
                self.push(*a);
                self.push(*b);
                self.f.byte(cmp_op(*op, *ty));
            }
            Expr::Select { cond, a, b, .. } => {
                self.push(*a);
                self.push(*b);
                self.push(*cond);
                self.f.select();
            }
            Expr::Wrap(a) => {
                self.push(*a);
                self.f.byte(code::I32_WRAP_I64);
            }
            Expr::Extend { a, signed } => {
                self.push(*a);
                self.f.byte(if *signed {
                    code::I64_EXTEND_I32_S
                } else {
                    code::I64_EXTEND_I32_U
                });
            }
            Expr::ExtendBits {
                a,
                ty,
                bits,
                signed,
            } => {
                self.push(*a);
                if *signed {
                    match (*ty, *bits) {
                        (Ty::I32, 8) => self.f.byte(code::I32_EXTEND8_S),
                        (Ty::I32, 16) => self.f.byte(code::I32_EXTEND16_S),
                        (Ty::I64, 8) => self.f.byte(code::I64_EXTEND8_S),
                        (Ty::I64, 16) => self.f.byte(code::I64_EXTEND16_S),
                        (Ty::I64, 32) => self.f.byte(code::I64_EXTEND32_S),
                        _ => {
                            // Widths the sign-extension opcodes do not cover:
                            // shift up and back down arithmetically.
                            let sh = ty.bits() - bits;
                            self.const_of(*ty, sh as u64);
                            self.f.byte(bin_op(BinOp::Shl, *ty));
                            self.const_of(*ty, sh as u64);
                            self.f.byte(bin_op(BinOp::ShrS, *ty));
                        }
                    }
                } else {
                    let mask = if *bits >= 64 {
                        u64::MAX
                    } else {
                        (1u64 << bits) - 1
                    };
                    self.const_of(*ty, mask);
                    self.f.byte(bin_op(BinOp::And, *ty));
                }
            }
            Expr::Load {
                addr,
                offset,
                sz,
                signed,
                ty,
                order,
            } => {
                let static_off = self.emit_address(*addr, *offset);
                if *order == Order::SeqCst {
                    // Atomic loads have no signed forms; the architecture has
                    // no signed load-acquire either.
                    let sub = atomic_load_op(*ty, *sz);
                    self.f.atomic(sub, natural_align(*sz), static_off);
                } else {
                    let opcode = load_op(*ty, *sz, *signed);
                    self.f.load(opcode, natural_align(*sz), static_off);
                }
            }
            Expr::Rmw {
                op,
                addr,
                val,
                sz,
                ty,
            } => {
                let static_off = self.emit_address(*addr, 0);
                self.push(*val);
                let sub = atomic_rmw_op(*op, *ty, *sz);
                self.f.atomic(sub, natural_align(*sz), static_off);
            }
            Expr::CmpXchg {
                addr,
                expected,
                replacement,
                sz,
                ty,
            } => {
                let static_off = self.emit_address(*addr, 0);
                self.push(*expected);
                self.push(*replacement);
                let sub = atomic_cmpxchg_op(*ty, *sz);
                self.f.atomic(sub, natural_align(*sz), static_off);
            }
            Expr::HostCall { func, args, .. } => {
                // Handled here so the result lands in a local like any other
                // definition.
                self.host_call_inline(*func, args);
            }

            Expr::VLoad {
                addr,
                offset,
                bytes,
            } => self.emit_vload(*addr, *offset, *bytes),
            Expr::VConst(b) => self.f.v128_const(b),
            Expr::VSplat { a, lane_bytes } => {
                self.push(*a);
                self.f.simd(match lane_bytes {
                    1 => simd::I8X16_SPLAT,
                    2 => simd::I16X8_SPLAT,
                    4 => simd::I32X4_SPLAT,
                    _ => simd::I64X2_SPLAT,
                });
            }
            Expr::VBin { op, a, b } => {
                self.push(*a);
                self.push(*b);
                self.f.simd(match op {
                    VBinOp::And => simd::V128_AND,
                    VBinOp::Or => simd::V128_OR,
                    VBinOp::Xor => simd::V128_XOR,
                    VBinOp::AndNot => simd::V128_ANDNOT,
                });
            }
            Expr::VLaneBin { op, lane, a, b } => {
                self.push(*a);
                self.push(*b);
                // The lifter refuses anything wasm cannot express, so this is
                // unreachable rather than a gap.
                match op.wasm_opcode(*lane) {
                    Some(sub) => self.f.simd(sub),
                    None => panic!("no wasm SIMD instruction for {op:?} on {lane:?}"),
                }
            }
            Expr::VLaneUn { op, lane, a } => {
                self.push(*a);
                match op.wasm_opcode(*lane) {
                    Some(sub) => self.f.simd(sub),
                    None => panic!("no wasm SIMD instruction for {op:?} on {lane:?}"),
                }
            }
            Expr::VShift {
                op,
                lane,
                a,
                amount,
            } => {
                self.push(*a);
                self.push(*amount);
                self.f.simd(lane_shift_op(*op, *lane));
            }
            Expr::VShuffle { a, b, lanes } => {
                // A lane index above 31 is not merely wrong, it makes the
                // module unloadable: V8 validates shuffle masks when it
                // compiles, so one bad mask anywhere rejects everything.
                debug_assert!(
                    lanes.iter().all(|l| *l < 32),
                    "shuffle mask out of range: {lanes:?}"
                );
                self.push(*a);
                self.push(*b);
                self.f.simd_shuffle(lanes);
            }
            Expr::VSwizzle { a, idx } => {
                self.push(*a);
                self.push(*idx);
                self.f.simd(simd::I8X16_SWIZZLE);
            }
            Expr::VBitSelect { mask, a, b } => {
                // wasm's bitselect takes (a, b, mask) and yields
                // (a & mask) | (b & !mask).
                self.push(*a);
                self.push(*b);
                self.push(*mask);
                self.f.simd(simd::V128_BITSELECT);
            }
            Expr::VCvt { op, from, a, b } => {
                self.push(*a);
                if let Some(b) = b {
                    self.push(*b);
                }
                self.f.simd(lane_cvt_op(*op, *from));
            }
            Expr::VUn { op, a } => {
                self.push(*a);
                match op {
                    VUnOp::PopcntBytes => self.f.simd(simd::I8X16_POPCNT),
                }
            }
            Expr::VExtract {
                a,
                lane,
                lane_bytes,
                signed,
                ..
            } => {
                self.push(*a);
                let sub = match (lane_bytes, signed) {
                    (1, true) => simd::I8X16_EXTRACT_LANE_S,
                    (1, false) => simd::I8X16_EXTRACT_LANE_U,
                    (2, true) => simd::I16X8_EXTRACT_LANE_S,
                    (2, false) => simd::I16X8_EXTRACT_LANE_U,
                    (4, _) => simd::I32X4_EXTRACT_LANE,
                    _ => simd::I64X2_EXTRACT_LANE,
                };
                self.f.simd_lane(sub, *lane as u8);
            }
            Expr::VReplace {
                a,
                lane,
                lane_bytes,
                val,
            } => {
                self.push(*a);
                self.push(*val);
                let sub = match lane_bytes {
                    1 => simd::I8X16_REPLACE_LANE,
                    2 => simd::I16X8_REPLACE_LANE,
                    4 => simd::I32X4_REPLACE_LANE,
                    _ => simd::I64X2_REPLACE_LANE,
                };
                self.f.simd_lane(sub, *lane as u8);
            }

            Expr::FpBin { op, double, a, b } => {
                self.push(*a);
                self.reinterpret_to_fp(*double);
                self.push(*b);
                self.reinterpret_to_fp(*double);
                let base: u8 = match op {
                    FpBinOp::Add => 0x92,
                    FpBinOp::Sub => 0x93,
                    FpBinOp::Mul => 0x94,
                    FpBinOp::Div => 0x95,
                    FpBinOp::Min => 0x96,
                    FpBinOp::Max => 0x97,
                    FpBinOp::Copysign => 0x98,
                };
                // The f64 arithmetic block sits 14 opcodes after the f32 one.
                self.f.byte(if *double { base + 0x0e } else { base });
                self.reinterpret_from_fp(*double);
            }
            Expr::FpUn { op, double, a } => {
                self.push(*a);
                self.reinterpret_to_fp(*double);
                let base: u8 = match op {
                    FpUnOp::Abs => 0x8b,
                    FpUnOp::Neg => 0x8c,
                    FpUnOp::Ceil => 0x8d,
                    FpUnOp::Floor => 0x8e,
                    FpUnOp::Trunc => 0x8f,
                    FpUnOp::RoundNearest => 0x90,
                    FpUnOp::Sqrt => 0x91,
                };
                self.f.byte(if *double { base + 0x0e } else { base });
                self.reinterpret_from_fp(*double);
            }
            Expr::FpCmp { op, double, a, b } => {
                self.push(*a);
                self.reinterpret_to_fp(*double);
                self.push(*b);
                self.reinterpret_to_fp(*double);
                let base: u8 = match op {
                    FpCmpOp::Eq => 0x5b,
                    FpCmpOp::Ne => 0x5c,
                    FpCmpOp::Lt => 0x5d,
                    FpCmpOp::Gt => 0x5e,
                    FpCmpOp::Le => 0x5f,
                    FpCmpOp::Ge => 0x60,
                };
                self.f.byte(if *double { base + 0x06 } else { base });
            }
            Expr::FpCvt {
                kind,
                src_wide,
                dst_wide,
                a,
            } => self.emit_fp_convert(*kind, *src_wide, *dst_wide, *a),
        }

        // Everything that reaches here left one value on the stack.
        let local = self.alloc(ty);
        self.f.local_set(local);
        self.value_local[idx] = Some(local);
    }

    /// Reinterpret the integer bit pattern on top of the stack as a float.
    fn reinterpret_to_fp(&mut self, double: bool) {
        // f64.reinterpret_i64 / f32.reinterpret_i32
        self.f.byte(if double { 0xbf } else { 0xbe });
    }

    /// Reinterpret the float on top of the stack back to its bit pattern.
    fn reinterpret_from_fp(&mut self, double: bool) {
        // i64.reinterpret_f64 / i32.reinterpret_f32
        self.f.byte(if double { 0xbd } else { 0xbc });
    }

    fn emit_fp_convert(&mut self, kind: FpConvert, src_wide: bool, dst_wide: bool, a: Value) {
        match kind {
            FpConvert::FpToFp => {
                self.push(a);
                self.reinterpret_to_fp(src_wide);
                // f32.demote_f64 / f64.promote_f32
                self.f.byte(if dst_wide { 0xbb } else { 0xb6 });
                self.reinterpret_from_fp(dst_wide);
            }
            FpConvert::IntToFp { signed } => {
                self.push(a);
                let op: u8 = match (dst_wide, src_wide, signed) {
                    (false, false, true) => 0xb2,  // f32.convert_i32_s
                    (false, false, false) => 0xb3, // f32.convert_i32_u
                    (false, true, true) => 0xb4,   // f32.convert_i64_s
                    (false, true, false) => 0xb5,  // f32.convert_i64_u
                    (true, false, true) => 0xb7,   // f64.convert_i32_s
                    (true, false, false) => 0xb8,  // f64.convert_i32_u
                    (true, true, true) => 0xb9,    // f64.convert_i64_s
                    (true, true, false) => 0xba,   // f64.convert_i64_u
                };
                self.f.byte(op);
                self.reinterpret_from_fp(dst_wide);
            }
            FpConvert::FpToInt { signed } => {
                self.push(a);
                self.reinterpret_to_fp(src_wide);
                // The *saturating* conversions, because AArch64's FCVTZ*
                // clamps out-of-range values and maps NaN to zero, while the
                // trapping forms would abort the guest.
                let sub: u32 = match (dst_wide, src_wide, signed) {
                    (false, false, true) => 0x00,
                    (false, false, false) => 0x01,
                    (false, true, true) => 0x02,
                    (false, true, false) => 0x03,
                    (true, false, true) => 0x04,
                    (true, false, false) => 0x05,
                    (true, true, true) => 0x06,
                    (true, true, false) => 0x07,
                };
                self.f.byte(0xfc);
                self.f.u32(sub);
            }
        }
    }

    /// Push a `ty`-typed constant.
    fn const_of(&mut self, ty: Ty, v: u64) {
        match ty {
            Ty::I32 => self.f.i32_const(v as i32),
            Ty::I64 => self.f.i64_const(v as i64),
            Ty::V128 => {
                let mut b = [0u8; 16];
                b[..8].copy_from_slice(&v.to_le_bytes());
                self.f.v128_const(&b);
            }
        }
    }

    /// A SIMD&FP load. Anything narrower than 16 bytes must zero the rest of
    /// the destination register, which is exactly what the `load*_zero` and
    /// `load*_lane`-into-zero forms give us.
    fn emit_vload(&mut self, addr: Value, offset: i64, bytes: u32) {
        let align = bytes.trailing_zeros();
        match bytes {
            16 => {
                let off = self.emit_address(addr, offset);
                self.f.simd_mem(simd::V128_LOAD, align, off);
            }
            8 => {
                let off = self.emit_address(addr, offset);
                self.f.simd_mem(simd::V128_LOAD64_ZERO, align, off);
            }
            4 => {
                let off = self.emit_address(addr, offset);
                self.f.simd_mem(simd::V128_LOAD32_ZERO, align, off);
            }
            // There is no `load8_zero`/`load16_zero`, so load into lane 0 of
            // an all-zero vector.
            _ => {
                let off = self.emit_address(addr, offset);
                self.f.v128_const(&[0u8; 16]);
                let sub = if bytes == 1 {
                    simd::V128_LOAD8_LANE
                } else {
                    simd::V128_LOAD16_LANE
                };
                self.f.simd_mem_lane(sub, align, off, 0);
            }
        }
    }

    fn type_of(&self, v: Value) -> Ty {
        self.value_ty.get(v.0 as usize).copied().unwrap_or(Ty::I64)
    }

    /// Emit a host call whose result stays on the stack.
    fn host_call_inline(&mut self, func: HostFunc, args: &[Value]) {
        if func.clobbers_guest_state() {
            self.flush();
        }
        self.f.local_get(self.ctx_local);
        for a in args {
            self.push(*a);
        }
        self.f.call(self.import_index(func));
        if func.clobbers_guest_state() {
            self.invalidate();
        }
    }

    fn host_call(&mut self, func: HostFunc, args: &[Value], _dst: Option<Value>, _b: &Block) {
        if func.clobbers_guest_state() {
            self.flush();
        }
        self.f.local_get(self.ctx_local);
        for a in args {
            self.push(*a);
        }
        self.f.call(self.import_index(func));
        if func.clobbers_guest_state() {
            self.invalidate();
        }
    }

    fn import_index(&self, func: HostFunc) -> u32 {
        match func {
            HostFunc::Syscall => self.cx.syscall,
            HostFunc::Illegal => self.cx.illegal,
            HostFunc::Breakpoint => self.cx.breakpoint,
            HostFunc::SysRegRead => self.cx.sysreg_read,
            HostFunc::SysRegWrite => self.cx.sysreg_write,
            HostFunc::CacheOp => self.cx.cache_op,
            HostFunc::Safepoint => self.cx.safepoint,
            HostFunc::Fma => self.cx.fma,
        }
    }

    // -- terminators -------------------------------------------------------

    fn terminator(&mut self, block: &Block) {
        match block.term {
            Term::Goto { target } => {
                if self.loop_depth.is_some() {
                    self.emit_loop_safepoint(target);
                    let d = self.loop_depth.unwrap();
                    self.f.br(d);
                    return;
                }
                self.flush();
                if target <= block.start {
                    self.emit_safepoint(target);
                }
                self.jump_to_constant(target);
            }
            Term::Branch {
                cond,
                taken,
                not_taken,
            } => {
                if let Some(depth) = self.loop_depth {
                    // One arm is the back-edge, which stays inside the loop
                    // and keeps every register in its local. Only the exit arm
                    // pays for a flush.
                    self.push(cond);
                    self.f.if_void();
                    if taken == block.start {
                        self.emit_loop_safepoint(taken);
                        self.f.br(depth + 1);
                    } else {
                        self.flush();
                        if taken <= block.start {
                            self.emit_safepoint(taken);
                        }
                        self.jump_to_constant(taken);
                    }
                    self.f.end();
                    if not_taken == block.start {
                        self.emit_loop_safepoint(not_taken);
                        self.f.br(depth);
                    } else {
                        self.flush();
                        if not_taken <= block.start {
                            self.emit_safepoint(not_taken);
                        }
                        self.jump_to_constant(not_taken);
                    }
                    return;
                }
                // Flush before the branch so both arms see the stores; doing
                // it inside the `if` would only cover the taken path.
                self.flush();
                // A conditional back-edge polls only on the taken path, so a
                // loop's exit test costs nothing extra.
                self.push(cond);
                self.f.if_void();
                if taken <= block.start {
                    self.emit_safepoint(taken);
                }
                self.jump_to_constant(taken);
                self.f.end();
                if not_taken <= block.start {
                    self.emit_safepoint(not_taken);
                }
                self.jump_to_constant(not_taken);
            }
            Term::Indirect { target, .. } => {
                self.push(target);
                self.f.local_set(self.scratch_i64);
                // The context PC must be exact before a possible dispatch
                // miss, since that is the only place the target is recorded.
                self.f.local_get(self.ctx_local);
                self.f.local_get(self.scratch_i64);
                self.f.store(0x37, 3, ctx::PC as u32);
                self.flush();
                self.f.local_get(self.ctx_local);
                self.emit_dispatch_index();
                self.f.return_call_indirect(self.cx.block_ty, 0);
            }
            Term::Syscall { next } => {
                self.flush();
                // The recorded PC is the instruction *after* the `svc`: it is
                // where a signal frame must resume, and resuming at the `svc`
                // itself would re-issue the syscall. The call still receives
                // the syscall's own address, which is what a trace wants.
                self.store_pc(next);
                self.f.local_get(self.ctx_local);
                self.f.i64_const((block.end - 4) as i64);
                self.f.call(self.cx.syscall);
                self.invalidate();
                self.jump_to_constant(next);
            }
            Term::Illegal { raw, pc } => {
                self.flush();
                self.store_pc(pc);
                self.f.local_get(self.ctx_local);
                self.f.i64_const(pc as i64);
                self.f.i32_const(raw as i32);
                self.f.call(self.cx.illegal);
                // `illegal` raises SIGILL and never returns.
                self.f.unreachable();
            }
        }
    }

    /// Poll for a pending signal before a backward jump.
    ///
    /// PLAN §5.6 puts safepoints on loop back-edges and a periodic counter.
    /// A backward direct branch is the cheap, sound approximation of a loop:
    /// it means no program can spin without reaching a poll, while
    /// straight-line code pays nothing. Forward branches and calls are left
    /// alone, and syscalls are already delivery points.
    fn emit_safepoint(&mut self, pc: u64) {
        // Count down, and only call out when the counter runs out, so the
        // common case is a load, a subtract, a store and a not-taken branch.
        self.f.local_get(self.ctx_local);
        self.f.local_get(self.ctx_local);
        self.f.load(0x28, 2, ctx::SAFEPOINT_COUNTER as u32);
        self.f.i32_const(1);
        self.f.byte(0x6b); // i32.sub
        self.f.local_tee(self.scratch_i32);
        self.f.store(0x36, 2, ctx::SAFEPOINT_COUNTER as u32);

        self.f.local_get(self.scratch_i32);
        self.f.i32_const(0);
        self.f.byte(0x4c); // i32.le_s
        self.f.if_void();
        // Inside a fused loop the registers live only in locals, so the slow
        // path -- which may deliver a signal and unwind -- has to write them
        // back. It runs once every few thousand iterations, so the cost does
        // not show up.
        // Inside a fused loop the registers live only in locals, so the slow
        // path -- which may deliver a signal and unwind -- has to write them
        // back. It runs once every few thousand iterations, so the cost does
        // not show up.
        if self.loop_depth.is_some() {
            self.flush_conditional();
        }
        self.f.local_get(self.ctx_local);
        self.f.i64_const(pc as i64);
        self.f.call(self.cx.safepoint);
        self.f.end();
    }

    /// The safepoint on a fused loop's own back-edge.
    ///
    /// Two differences from [`Self::emit_safepoint`], both of which matter
    /// only because a fused loop is the one place gex emits that a guest can
    /// spend all its time inside.
    ///
    /// The countdown is a local rather than the counter in the context, so
    /// the hot path is a subtract and a branch with no memory traffic at all.
    /// The context's copy is only reloaded on entry and written back on the
    /// way out, which keeps the cadence honest across entries.
    ///
    /// And when it expires the loop does not resume: it goes back through the
    /// dispatcher, which re-enters this same block. That is for V8, which is
    /// what a page runs. Its baseline compiler handles a module first and
    /// promotes a function to the optimising one on a budget that counts
    /// *calls* -- so a loop entered once and iterated a hundred million times
    /// is never promoted, and the hottest code in the program stays baseline
    /// forever. Measured on a 300M-iteration loop: 16.9s without this, 7.8s
    /// with it.
    fn emit_loop_safepoint(&mut self, pc: u64) {
        let Some(counter) = self.reenter_local else {
            self.emit_safepoint(pc);
            return;
        };
        self.f.local_get(counter);
        self.f.i32_const(1);
        self.f.byte(0x6b); // i32.sub
        self.f.local_tee(counter);
        self.f.i32_const(0);
        self.f.byte(0x4c); // i32.le_s
        self.f.if_void();
        self.flush_conditional();
        // The host rewrites the context's counter, so the next entry starts a
        // fresh interval without this having to say so.
        self.f.local_get(self.ctx_local);
        self.f.i64_const(pc as i64);
        self.f.call(self.cx.safepoint);
        self.jump_to_constant(pc);
        self.f.end();
    }

    /// Transfer control to a statically known guest address.
    fn jump_to_constant(&mut self, target: u64) {
        if let Some(&func) = self.cx.blocks.get(&target) {
            self.f.local_get(self.ctx_local);
            self.f.return_call(func);
            return;
        }
        // Not in this module: record the PC and go through the table. The
        // page's directory entry decides whether that reaches real code or
        // the miss stub, and it can change as translation proceeds, so even a
        // constant target has to be looked up at run time.
        self.store_pc(target);
        self.f.local_get(self.ctx_local);
        self.f.i64_const(target as i64);
        self.f.local_set(self.scratch_i64);
        self.emit_dispatch_index();
        self.f.return_call_indirect(self.cx.block_ty, 0);
    }

    /// Consume the target address in `scratch_i64` and leave a dispatch-table
    /// index on the stack.
    ///
    /// Two levels, and deliberately branchless. A page with no translation
    /// has a directory entry of zero, so the index lands in the reserved
    /// miss-stub span at the bottom of the table and the stub asks the host
    /// to translate. An address beyond the directory's reach selects zero the
    /// same way, so it needs no separate test either.
    fn emit_dispatch_index(&mut self) {
        // page = pc >> 12
        self.f.local_get(self.scratch_i64);
        self.f.i64_const(DISPATCH_PAGE.trailing_zeros() as i64);
        self.f.byte(0x88); // i64.shr_u
        self.f.local_set(self.scratch_page);

        // entry = page < MAX_PAGES ? load(DISPATCH_DIR + page*4) : 0
        self.f.local_get(self.scratch_page);
        self.f.byte(0xa7); // i32.wrap_i64
        self.f.i32_const(4);
        self.f.byte(0x6c); // i32.mul
        self.f.load(0x28, 2, DISPATCH_DIR as u32);
        self.f.i32_const(0);
        self.f.local_get(self.scratch_page);
        self.f.i64_const(DISPATCH_MAX_PAGES as i64);
        self.f.byte(code::I64_LT_U);
        self.f.select();

        // index = entry + ((pc & 0xfff) >> 2)
        self.f.local_get(self.scratch_i64);
        self.f.i64_const((DISPATCH_PAGE - 1) as i64);
        self.f.byte(0x83); // i64.and
        self.f.i64_const(2);
        self.f.byte(0x88); // i64.shr_u
        self.f.byte(0xa7); // i32.wrap_i64
        self.f.byte(0x6a); // i32.add
    }
}

/// Index of the last statement that reads each value, or `None` when the
/// value is read by the terminator (and so must survive the whole block).
fn compute_last_use(block: &Block) -> Vec<Option<usize>> {
    let n = block.n_values as usize;
    // A value with no reader at all is freed immediately after its own
    // definition, which is why the default is the defining statement.
    let mut last: Vec<Option<usize>> = vec![Some(0); n];
    for (i, stmt) in block.stmts.iter().enumerate() {
        if let Stmt::Def { dst, .. } = stmt {
            if let Some(slot) = last.get_mut(dst.0 as usize) {
                *slot = Some(i);
            }
        }
    }
    for (i, stmt) in block.stmts.iter().enumerate() {
        stmt.for_each_use(|v| {
            if let Some(slot) = last.get_mut(v.0 as usize) {
                *slot = Some(i);
            }
        });
    }
    // Values the terminator reads must survive to the end of the block.
    block.term_uses(|v| {
        if let Some(slot) = last.get_mut(v.0 as usize) {
            *slot = None;
        }
    });
    last
}

// ---------------------------------------------------------------------------
// Opcode tables
// ---------------------------------------------------------------------------

fn bin_op(op: BinOp, ty: Ty) -> u8 {
    let base32: u8 = match op {
        BinOp::Add => 0x6a,
        BinOp::Sub => 0x6b,
        BinOp::Mul => 0x6c,
        BinOp::DivS => 0x6d,
        BinOp::DivU => 0x6e,
        BinOp::RemS => 0x6f,
        BinOp::RemU => 0x70,
        BinOp::And => 0x71,
        BinOp::Or => 0x72,
        BinOp::Xor => 0x73,
        BinOp::Shl => 0x74,
        BinOp::ShrS => 0x75,
        BinOp::ShrU => 0x76,
        BinOp::Rotr => 0x78,
    };
    match ty {
        Ty::I32 => base32,
        // The i64 arithmetic block starts 18 opcodes after the i32 one and
        // holds the same operations in the same order. Vectors never reach
        // here: they use the dedicated `VBin` expression.
        Ty::I64 | Ty::V128 => base32 + 0x12,
    }
}

fn cmp_op(op: CmpOp, ty: Ty) -> u8 {
    let base32: u8 = match op {
        CmpOp::Eq => 0x46,
        CmpOp::Ne => 0x47,
        CmpOp::LtS => 0x48,
        CmpOp::LtU => 0x49,
        CmpOp::GtS => 0x4a,
        CmpOp::GtU => 0x4b,
        CmpOp::LeS => 0x4c,
        CmpOp::LeU => 0x4d,
        CmpOp::GeS => 0x4e,
        CmpOp::GeU => 0x4f,
    };
    match ty {
        Ty::I32 => base32,
        // The i64 comparison block mirrors the i32 one, 11 opcodes later.
        Ty::I64 | Ty::V128 => base32 + 0x0b,
    }
}

/// Wasm SIMD sub-opcode for a lane-wise binary operation.
///
/// The tables are irregular enough — `i64x2` has no unsigned comparisons, no
/// min/max and no saturating forms — that spelling each case out is clearer
/// than deriving them from a base plus an offset.

fn lane_shift_op(op: VShiftOp, lane: VLane) -> u32 {
    let base = match lane {
        VLane::I8 => 0x6b,
        VLane::I16 => 0x8b,
        VLane::I32 | VLane::F32 => 0xab,
        _ => 0xcb,
    };
    base + match op {
        VShiftOp::Shl => 0,
        VShiftOp::ShrS => 1,
        VShiftOp::ShrU => 2,
    }
}

/// Whether a statement can leave the register cache stale.
thread_local! {
    /// Bytes emitted moving guest registers between the context and locals,
    /// for `--verbose`. That traffic is the price of a block boundary.
    pub static CTX_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Iterations of a fused loop between trips through the dispatcher.
///
/// The same cadence the context's safepoint counter uses off a fused loop, so
/// the time a guest can spend between signal polls does not depend on which
/// shape its loop happened to take. Overridable to bisect a suspected
/// scheduling problem; the value made no measurable difference anywhere
/// between 1024 and a million.
fn loop_poll() -> i32 {
    static POLL: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    *POLL.get_or_init(|| {
        std::env::var("GEX_LOOP_POLL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1024)
    })
}

fn clobbers_guest_state(s: &Stmt) -> bool {
    match s {
        Stmt::HostCall { func, .. } => func.clobbers_guest_state(),
        Stmt::Def {
            expr: Expr::HostCall { func, .. },
            ..
        } => func.clobbers_guest_state(),
        _ => false,
    }
}

/// The architectural registers a block reads before writing.
///
/// These are exactly the ones the fused loop form must load before the loop:
/// inside it the context is deliberately left stale, so a read that reaches
/// past the loop's top would see a value one or more iterations old. A
/// register the block writes before reading needs no preload, and leaving it
/// out matters -- every preloaded register is live across the whole loop, and
/// wasm locals that stay live become machine registers the host's allocator
/// cannot use for anything else.
fn live_in_registers(block: &Block) -> Vec<GuestReg> {
    let mut live_in = Vec::new();
    let mut written = Vec::new();
    for s in &block.stmts {
        match s {
            Stmt::Def {
                expr: Expr::ReadReg(r),
                ..
            } => {
                if !written.contains(r) && !live_in.contains(r) {
                    live_in.push(*r);
                }
            }
            Stmt::WriteReg { reg, .. } => {
                if !written.contains(reg) {
                    written.push(*reg);
                }
            }
            _ => {}
        }
    }
    live_in
}

fn lane_cvt_op(op: VCvtOp, from: VLane) -> u32 {
    match (op, from) {
        (VCvtOp::FpToIntSat { signed: true }, VLane::F32) => 0xf8,
        (VCvtOp::FpToIntSat { signed: false }, VLane::F32) => 0xf9,
        // The f64 forms produce two lanes and zero the upper two.
        (VCvtOp::FpToIntSat { signed: true }, VLane::F64) => 0xfc,
        (VCvtOp::FpToIntSat { signed: false }, VLane::F64) => 0xfd,
        (VCvtOp::IntToFp { signed: true }, VLane::I32) => 0xfa,
        (VCvtOp::IntToFp { signed: false }, VLane::I32) => 0xfb,
        // wasm has no i64x2 <-> f64x2 conversion, so the lifter must never
        // ask for one; `f64x2.convert_low_i32x4_s` is not a substitute.
        (VCvtOp::IntToFp { signed: true }, VLane::I64)
        | (VCvtOp::IntToFp { signed: false }, VLane::I64)
        | (VCvtOp::FpToIntSat { .. }, _) => {
            panic!("no wasm SIMD conversion for {op:?} from {from:?}")
        }
        (VCvtOp::IntToFp { signed: true }, _) => 0xfe,
        (VCvtOp::IntToFp { signed: false }, _) => 0xff,
        (VCvtOp::DemoteF64, _) => 0x5e,
        (VCvtOp::PromoteF32, _) => 0x5f,
        (VCvtOp::ExtendLow { signed: true }, VLane::I8) => 0x87,
        (VCvtOp::ExtendHigh { signed: true }, VLane::I8) => 0x88,
        (VCvtOp::ExtendLow { signed: false }, VLane::I8) => 0x89,
        (VCvtOp::ExtendHigh { signed: false }, VLane::I8) => 0x8a,
        (VCvtOp::ExtendLow { signed: true }, VLane::I16) => 0xa7,
        (VCvtOp::ExtendHigh { signed: true }, VLane::I16) => 0xa8,
        (VCvtOp::ExtendLow { signed: false }, VLane::I16) => 0xa9,
        (VCvtOp::ExtendHigh { signed: false }, VLane::I16) => 0xaa,
        (VCvtOp::ExtendLow { signed: true }, _) => 0xc7,
        (VCvtOp::ExtendHigh { signed: true }, _) => 0xc8,
        (VCvtOp::ExtendLow { signed: false }, _) => 0xc9,
        (VCvtOp::ExtendHigh { signed: false }, _) => 0xca,
        (VCvtOp::NarrowSat { signed: true }, VLane::I16) => 0x65,
        (VCvtOp::NarrowSat { signed: false }, VLane::I16) => 0x66,
        (VCvtOp::NarrowSat { signed: true }, _) => 0x85,
        (VCvtOp::NarrowSat { signed: false }, _) => 0x86,
    }
}

fn natural_align(sz: MemSz) -> u32 {
    match sz {
        MemSz::B1 => 0,
        MemSz::B2 => 1,
        MemSz::B4 => 2,
        MemSz::B8 => 3,
    }
}

fn load_op(ty: Ty, sz: MemSz, signed: bool) -> u8 {
    match (ty, sz, signed) {
        (Ty::I32, MemSz::B1, false) => 0x2d,
        (Ty::I32, MemSz::B1, true) => 0x2c,
        (Ty::I32, MemSz::B2, false) => 0x2f,
        (Ty::I32, MemSz::B2, true) => 0x2e,
        (Ty::I32, _, _) => 0x28,
        (Ty::I64, MemSz::B1, false) => 0x31,
        (Ty::I64, MemSz::B1, true) => 0x30,
        (Ty::I64, MemSz::B2, false) => 0x33,
        (Ty::I64, MemSz::B2, true) => 0x32,
        (Ty::I64, MemSz::B4, false) => 0x35,
        (Ty::I64, MemSz::B4, true) => 0x34,
        (Ty::I64, MemSz::B8, _) => 0x29,
        // Vector accesses go through `VLoad`/`VStore`, never this table.
        (Ty::V128, _, _) => unreachable!("vector load reached the scalar table"),
    }
}

fn store_op(ty: Ty, sz: MemSz) -> u8 {
    match (ty, sz) {
        (Ty::I32, MemSz::B1) => 0x3a,
        (Ty::I32, MemSz::B2) => 0x3b,
        (Ty::I32, _) => 0x36,
        (Ty::I64, MemSz::B1) => 0x3c,
        (Ty::I64, MemSz::B2) => 0x3d,
        (Ty::I64, MemSz::B4) => 0x3e,
        (Ty::I64, MemSz::B8) => 0x37,
        (Ty::V128, _) => unreachable!("vector store reached the scalar table"),
    }
}

/// Position of a `(type, size)` pair within an atomic opcode group.
///
/// Every atomic group in the threads proposal lists its seven variants in the
/// same order: `i32`, `i64`, `i32.8`, `i32.16`, `i64.8`, `i64.16`, `i64.32`.
fn atomic_slot(ty: Ty, sz: MemSz) -> u32 {
    match (ty, sz) {
        (Ty::I32, MemSz::B4) => 0,
        (Ty::I64, MemSz::B8) => 1,
        (Ty::I32, MemSz::B1) => 2,
        (Ty::I32, MemSz::B2) => 3,
        (Ty::I64, MemSz::B1) => 4,
        (Ty::I64, MemSz::B2) => 5,
        (Ty::I64, MemSz::B4) => 6,
        // A 32-bit value with an 8-byte access cannot occur; treat it as the
        // widest i32 form so the mapping stays total.
        (Ty::I32, MemSz::B8) => 0,
        // There are no atomic vector accesses in the architecture.
        (Ty::V128, _) => unreachable!("atomic vector access"),
    }
}

fn atomic_load_op(ty: Ty, sz: MemSz) -> u32 {
    0x10 + atomic_slot(ty, sz)
}

fn atomic_store_op(ty: Ty, sz: MemSz) -> u32 {
    0x17 + atomic_slot(ty, sz)
}

fn atomic_rmw_op(op: RmwOp, ty: Ty, sz: MemSz) -> u32 {
    let base = match op {
        RmwOp::Add => 0x1e,
        RmwOp::Sub => 0x25,
        RmwOp::And => 0x2c,
        RmwOp::Or => 0x33,
        RmwOp::Xor => 0x3a,
        RmwOp::Xchg => 0x41,
    };
    base + atomic_slot(ty, sz)
}

fn atomic_cmpxchg_op(ty: Ty, sz: MemSz) -> u32 {
    0x48 + atomic_slot(ty, sz)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_groups_are_seven_wide() {
        assert_eq!(atomic_rmw_op(RmwOp::Add, Ty::I32, MemSz::B4), 0x1e);
        assert_eq!(atomic_rmw_op(RmwOp::Add, Ty::I64, MemSz::B4), 0x24);
        assert_eq!(atomic_rmw_op(RmwOp::Sub, Ty::I32, MemSz::B4), 0x25);
        assert_eq!(atomic_cmpxchg_op(Ty::I64, MemSz::B8), 0x49);
    }

    #[test]
    fn integer_opcodes_match_the_spec() {
        assert_eq!(bin_op(BinOp::Add, Ty::I32), 0x6a);
        assert_eq!(bin_op(BinOp::Add, Ty::I64), 0x7c);
        assert_eq!(bin_op(BinOp::Rotr, Ty::I64), 0x8a);
        assert_eq!(cmp_op(CmpOp::Eq, Ty::I64), 0x51);
        assert_eq!(cmp_op(CmpOp::GeU, Ty::I64), 0x5a);
    }

    #[test]
    fn dispatch_indices_follow_the_page_directory() {
        // A page whose directory entry is `base` maps its instructions to
        // consecutive slots from there.
        assert_eq!(gex_abi::dispatch_index(0x400000, 2048), 2048);
        assert_eq!(gex_abi::dispatch_index(0x400004, 2048), 2049);
        assert_eq!(gex_abi::dispatch_index(0x400ffc, 2048), 2048 + 1023);
        // The next page starts over from its own entry.
        assert_eq!(gex_abi::dispatch_index(0x401000, 4096), 4096);
        // A page with no translation lands in the reserved miss span.
        assert!(gex_abi::dispatch_index(0x400004, 0) < gex_abi::MISS_SLOTS);
    }
}
