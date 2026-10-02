//! Function mode: tier-1's statement lowering for whole functions, for the
//! function JIT (`gex-fjit`, docs/merge-plan.md §9.8).
//!
//! The JIT owns the control flow -- structured `block`/`loop`/`br` over the
//! function's blocks, calls, the `F_body` ABI. Everything inside a block
//! (addressing and the bounds guard, atomics, flags, FP, x87, SIMD) is
//! tier-1's own code, so memory semantics, flags and FP are tier-1's bit for
//! bit. What differs is the register cache: every register the function uses
//! has one local for the whole body, loaded on entry and again wherever guest
//! state may have changed, and the JIT says which registers may differ from
//! the context ([`FnBody::set_dirty`]) and which to write back where
//! ([`FnBody::flush`]), as gex-cgen's function mode does in C.

use super::*;
use gex_ir::func::RegSet;

/// One function body under construction.
pub struct FnBody<'a> {
    e: Emitter<'a>,
    /// Per value: the index of its last use in the block being emitted
    /// (`usize::MAX`: kept past the statements). Scratch for [`Self::block`].
    last: Vec<usize>,
}

impl<'a> FnBody<'a> {
    /// A function of `n_params` parameters (`ctx` first) whose blocks use
    /// the registers `used` and number their values `0..n_values` across
    /// the function. No code yet: the caller loads the registers
    /// ([`Self::load`]) where it wants them loaded.
    pub fn new(cx: &'a Context<'a>, start: u64, n_params: u32, used: &[GuestReg], n_values: u32) -> FnBody<'a> {
        let mut e = Emitter::with_params(cx, start, n_params);
        let mut used: Vec<GuestReg> = used.to_vec();
        used.sort_by_key(|r| Emitter::reg_offset(*r));
        used.dedup();
        for &r in &used {
            let local = e.f.add_local(val_type(r.ty()));
            e.regs.insert(r, Cached { local, dirty: false });
        }
        e.func_regs = Some(used);
        let n = n_values as usize;
        e.value_local = vec![None; n];
        e.value_const = vec![None; n];
        e.value_ty = vec![Ty::I64; n];
        FnBody { e, last: vec![usize::MAX; n] }
    }

    /// The code, for the JIT's own control flow.
    pub fn f(&mut self) -> &mut FuncBody {
        &mut self.e.f
    }

    /// The `ctx` parameter.
    pub fn ctx(&self) -> u32 {
        self.e.ctx_local
    }

    /// A fresh local of the JIT's own.
    pub fn local(&mut self, ty: ValType) -> u32 {
        self.e.f.add_local(ty)
    }

    /// The local holding `r` (which must be in the used set).
    pub fn reg(&self, r: GuestReg) -> u32 {
        self.e.regs[&r].local
    }

    pub fn is_used(&self, r: GuestReg) -> bool {
        self.e.regs.contains_key(&r)
    }

    /// Load `regs` from the context into their locals; they are clean.
    pub fn load(&mut self, regs: &[GuestReg]) {
        self.e.reload(regs);
    }

    /// Reload every used register outside `keep` (whose locals hold what
    /// the context does, e.g. a callee's preserved registers); afterwards
    /// none is dirty.
    pub fn reload_except(&mut self, keep: RegSet) {
        let used = self.e.func_regs.clone().unwrap_or_default();
        let regs: Vec<GuestReg> = used.into_iter().filter(|r| !keep.contains(*r)).collect();
        self.e.reload(&regs);
        self.set_dirty(RegSet::EMPTY);
    }

    /// The registers that may differ from the context from here on.
    pub fn set_dirty(&mut self, dirty: RegSet) {
        for (r, c) in self.e.regs.iter_mut() {
            c.dirty = dirty.contains(*r);
        }
    }

    pub fn dirty(&self) -> RegSet {
        let mut d = RegSet::EMPTY;
        for (r, c) in &self.e.regs {
            if c.dirty {
                d.insert(*r);
            }
        }
        d
    }

    /// Write back the dirty registers in `mask` (tier-1's stores, flags
    /// packed as tier-1 packs them). Unless `keep_dirty`, every register is
    /// clean afterwards: the ones outside `mask` are dead (nothing reads
    /// them before they are written again), as gex-cgen's function mode
    /// treats them.
    pub fn flush(&mut self, mask: RegSet, keep_dirty: bool) {
        let before = self.dirty();
        self.set_dirty(before.inter(mask));
        self.e.flush_inner(true);
        self.set_dirty(if keep_dirty { before } else { RegSet::EMPTY });
    }

    /// Emit one block's statements. `start` is the block's guest address
    /// (what a fault inside reports); values in `keep` stay live after the
    /// statements, for the terminator, until [`Self::release`].
    pub fn block(&mut self, start: u64, stmts: &[Stmt], keep: &[Value]) {
        self.e.block_start = start;
        // Last uses, over the values this block touches only: values are
        // numbered across the function but local to their block.
        let mut touched: Vec<u32> = Vec::new();
        for (i, s) in stmts.iter().enumerate() {
            let last = &mut self.last;
            let mut note = |v: Value| {
                let k = v.0 as usize;
                if last[k] == usize::MAX {
                    touched.push(v.0);
                }
                last[k] = i;
            };
            if let Stmt::Def { dst, .. } = s {
                note(*dst);
            }
            s.for_each_use(&mut note);
        }
        let mut frees: Vec<(usize, u32)> = Vec::new();
        for v in touched {
            let at = std::mem::replace(&mut self.last[v as usize], usize::MAX);
            if !keep.iter().any(|k| k.0 == v) {
                frees.push((at, v));
            }
        }
        frees.sort_unstable();
        let dummy = Block::new(start);
        let mut next = 0;
        for (i, s) in stmts.iter().enumerate() {
            self.e.stmt(s, &dummy);
            while next < frees.len() && frees[next].0 == i {
                self.e.release(Value(frees[next].1));
                next += 1;
            }
        }
    }

    /// Free the locals of values kept past [`Self::block`].
    pub fn release(&mut self, vals: &[Value]) {
        for v in vals {
            self.e.release(*v);
        }
    }

    /// Push a value of the current block.
    pub fn push(&mut self, v: Value) {
        self.e.push(v);
    }

    /// A value's constant, when it has one.
    pub fn constant(&self, v: Value) -> Option<u64> {
        self.e.value_const.get(v.0 as usize).copied().flatten()
    }

    pub fn store_pc(&mut self, pc: u64) {
        self.e.store_pc(pc);
    }

    /// tier-1's safepoint countdown (the context's counter), polling with
    /// `pc`. Writes nothing back: the caller flushes first if it must.
    pub fn safepoint(&mut self, pc: u64) {
        self.e.emit_safepoint(pc);
    }

    /// The same, with the pc in the i64 local `pc`.
    pub fn safepoint_dynamic(&mut self, pc: u32) {
        let ctx = self.e.ctx_local;
        let sc = self.e.scratch_i32;
        let f = &mut self.e.f;
        f.local_get(ctx);
        f.local_get(ctx);
        f.load(0x28, 2, ctx::SAFEPOINT_COUNTER as u32);
        f.i32_const(1);
        f.byte(0x6b); // i32.sub
        f.local_tee(sc);
        f.store(0x36, 2, ctx::SAFEPOINT_COUNTER as u32);
        f.local_get(sc);
        f.i32_const(0);
        f.byte(0x4c); // i32.le_s
        f.if_void();
        f.local_get(ctx);
        f.local_get(pc);
        f.call(self.e.cx.safepoint);
        f.end();
    }

    pub fn finish(self) -> Vec<u8> {
        self.e.f.finish()
    }
}

/// A register's context offset, as tier-1 stores it.
pub fn reg_offset(r: GuestReg) -> u32 {
    Emitter::reg_offset(r)
}

impl Emitter<'_> {
    /// Function mode: load `regs` into their locals, marking them clean.
    pub(super) fn reload(&mut self, regs: &[GuestReg]) {
        for &r in regs {
            let Some(c) = self.regs.get_mut(&r) else { continue };
            c.dirty = false;
            let local = c.local;
            self.f.local_get(self.ctx_local);
            if r == GuestReg::CtxAddr {
                self.f.byte(code::I64_EXTEND_I32_U);
            } else if Self::is_flag(r) {
                self.f.load(0x2d, 0, Self::reg_offset(r)); // i32.load8_u
            } else {
                match r.ty() {
                    Ty::I32 => self.f.load(0x28, 2, Self::reg_offset(r)),
                    Ty::I64 => self.f.load(0x29, 3, Self::reg_offset(r)),
                    Ty::V128 => self.f.simd_mem(simd::V128_LOAD, 4, Self::reg_offset(r)),
                }
            }
            self.f.local_set(local);
        }
    }
}
