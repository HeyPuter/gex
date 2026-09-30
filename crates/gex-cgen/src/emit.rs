//! GIR block → C function, with tier-1's semantics statement for statement.
//!
//! Phase A of the function tier (`docs/function-tier.md`): every block is one
//! C function `void b_<addr>(int ctx)`, the uniform signature. The register
//! cache, the write-backs, the safepoints and the dispatch follow
//! `gex_emit::tier1` exactly; only the code generator differs. Where tier-1
//! emits a wasm instruction, this emits the C that clang lowers to it.

use anyhow::{bail, Result};
use gex_abi::{ctx, GUEST_LIMIT};
use gex_ir::func::RegSet;
use gex_ir::*;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write;

/// The C name of the function for a block at `addr`.
pub fn fn_name(addr: u64) -> String {
    format!("b_{addr:x}")
}

/// Refuse the tier-1 diagnostics the function tier does not reproduce, so a
/// run that asked for one does not silently go without it.
pub fn check_environment() -> Result<()> {
    for var in [
        "GEX_CHECK_SP",
        "GEX_TRAP_PC",
        "GEX_TRACE_BLOCKS",
        "GEX_COHERENT_LOADS",
        "GEX_COPY_LOOP",
        "GEX_NO_DIRECT_JUMP",
        "GEX_DISPATCH_BALLAST",
    ] {
        if std::env::var_os(var).is_some() {
            bail!("{var} is set, and the function tier (--cgen) does not implement it");
        }
    }
    Ok(())
}

fn ballast() -> usize {
    std::env::var("GEX_CGEN_BALLAST")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

fn no_guard() -> bool {
    std::env::var_os("GEX_UNSAFE_NO_GUARD").is_some()
}

pub(crate) fn cty(ty: Ty) -> &'static str {
    match ty {
        Ty::I32 => "uint32_t",
        Ty::I64 => "uint64_t",
        Ty::V128 => "v128_t",
    }
}

pub(crate) fn reg_name(r: GuestReg) -> String {
    match r {
        GuestReg::X(n) => format!("r_x{n}"),
        GuestReg::V128(n) => format!("r_v{n}"),
        GuestReg::Sp => "r_sp".into(),
        GuestReg::Pc => "r_pc".into(),
        GuestReg::N => "f_n".into(),
        GuestReg::Z => "f_z".into(),
        GuestReg::C => "f_c".into(),
        GuestReg::V => "f_v".into(),
        GuestReg::P => "f_p".into(),
        GuestReg::A => "f_a".into(),
        GuestReg::D => "f_d".into(),
        GuestReg::Tpidr => "r_tpidr".into(),
        GuestReg::TpidrRo => "r_tpidrro".into(),
        GuestReg::MonAddr => "r_monaddr".into(),
        GuestReg::MonValue => "r_monvalue".into(),
        GuestReg::MonSize => "r_monsize".into(),
        GuestReg::Mxcsr => "r_mxcsr".into(),
        GuestReg::X87Top => "r_x87top".into(),
        GuestReg::X87Cw => "r_x87cw".into(),
        GuestReg::X87Sw => "r_x87sw".into(),
        GuestReg::X87Tags => "r_x87tags".into(),
        GuestReg::CtxAddr => "r_ctxaddr".into(),
    }
}

/// `tier1::Emitter::reg_offset`.
pub(crate) fn reg_offset(r: GuestReg) -> usize {
    match r {
        GuestReg::X(n) => ctx::x(n),
        GuestReg::V128(n) => ctx::v(n),
        GuestReg::Sp => ctx::SP,
        GuestReg::Pc => ctx::PC,
        GuestReg::N => ctx::NF,
        GuestReg::Z => ctx::ZF,
        GuestReg::C => ctx::CF,
        GuestReg::V => ctx::VF,
        GuestReg::P => ctx::PF,
        GuestReg::A => ctx::AF,
        GuestReg::D => ctx::DF,
        GuestReg::Tpidr => ctx::TPIDR,
        GuestReg::TpidrRo => ctx::TPIDRRO,
        GuestReg::MonAddr => ctx::MON_ADDR,
        GuestReg::MonValue => ctx::MON_VALUE,
        GuestReg::MonSize => ctx::MON_SIZE,
        GuestReg::Mxcsr => ctx::MXCSR,
        GuestReg::X87Top => ctx::X87_TOP,
        GuestReg::X87Cw => ctx::X87_CW,
        GuestReg::X87Sw => ctx::X87_SW,
        GuestReg::X87Tags => ctx::X87_TAGS,
        GuestReg::CtxAddr => 0,
    }
}

/// C for one translation unit: the header include, prototypes for every block
/// function referenced, and the bodies.
pub struct Unit {
    protos: BTreeSet<u64>,
    bodies: String,
}

impl Default for Unit {
    fn default() -> Self {
        Self::new()
    }
}

impl Unit {
    pub fn new() -> Unit {
        Unit {
            protos: BTreeSet::new(),
            bodies: String::new(),
        }
    }

    /// Add the C function for `block`. `owned` is every block address with a
    /// C body in this module, for direct `musttail` successors.
    pub fn add_block(&mut self, block: &Block, owned: &HashSet<u64>) -> Result<()> {
        let mut e = Emitter::new(block, owned);
        e.run()?;
        self.protos.insert(block.start);
        self.protos.extend(e.called.iter().copied());
        let _ = writeln!(self.bodies, "void {}(int ctx) {{", fn_name(block.start));
        for (r, _) in e.declared.iter() {
            let _ = writeln!(self.bodies, "  {} {};", cty(r.ty()), reg_name(*r));
        }
        self.bodies.push_str(&e.out);
        self.bodies.push_str("}\n\n");
        Ok(())
    }

    /// The finished C source.
    pub fn source(&self) -> String {
        let mut s = String::from("#include \"gex_abi.h\"\n\n");
        for a in &self.protos {
            let _ = writeln!(s, "void {}(int ctx);", fn_name(*a));
        }
        s.push('\n');
        s.push_str(&self.bodies);
        s
    }
}

/// Function mode (Phase B): every register the function touches is a C
/// local for the whole function, loaded at entry and after anything that may
/// change guest state, and written back at every point control can leave.
pub(crate) struct FnRegs {
    /// Read or written anywhere: loaded on entry and reloaded after calls.
    pub used: BTreeSet<GuestReg>,
    /// Written anywhere: stored back before control can leave.
    pub written: BTreeSet<GuestReg>,
    /// Store only the registers that may be dirty at each write-back.
    pub clean_spills: bool,
}

pub(crate) struct Emitter<'a> {
    block: &'a Block,
    /// Set in function mode; `None` for tier-1-style blocks.
    pub(crate) func_regs: Option<FnRegs>,
    owned: &'a HashSet<u64>,
    out: String,
    /// Cached registers and whether each is dirty (tier-1's `regs`).
    regs: BTreeMap<GuestReg, bool>,
    /// Every register that has a C variable (declared at the top).
    declared: BTreeMap<GuestReg, ()>,
    value_ty: Vec<Ty>,
    value_const: Vec<Option<u64>>,
    /// Values known to fit in 32 bits because they are `x & 0xffffffff` or
    /// `x >> 32` -- the halves of a lifted `umulh`/`smulh`. See `bin`.
    value_half: Vec<bool>,
    /// Blocks tail-called directly, for prototypes.
    called: BTreeSet<u64>,
    /// Counter for block-scoped temporaries.
    tmp: u32,
}

impl<'a> Emitter<'a> {
    pub(crate) fn new(block: &'a Block, owned: &'a HashSet<u64>) -> Emitter<'a> {
        let n = block.n_values as usize;
        Emitter {
            block,
            func_regs: None,
            owned,
            out: String::new(),
            regs: BTreeMap::new(),
            declared: BTreeMap::new(),
            value_ty: vec![Ty::I64; n],
            value_const: vec![None; n],
            value_half: vec![false; n],
            called: BTreeSet::new(),
            tmp: 0,
        }
    }

    /// Size the per-value tables for `n` values (function mode numbers
    /// values across the whole function).
    pub(crate) fn resize(&mut self, n: usize) {
        self.value_ty = vec![Ty::I64; n];
        self.value_const = vec![None; n];
        self.value_half = vec![false; n];
    }

    /// The C emitted so far, leaving the emitter empty.
    pub(crate) fn take_output(&mut self) -> String {
        std::mem::take(&mut self.out)
    }

    pub(crate) fn line(&mut self, s: impl AsRef<str>) {
        self.out.push_str("  ");
        self.out.push_str(s.as_ref());
        self.out.push('\n');
    }

    pub(crate) fn fresh(&mut self) -> String {
        self.tmp += 1;
        format!("t{}", self.tmp)
    }

    pub(crate) fn v(&self, v: Value) -> String {
        format!("v{}", v.0)
    }

    fn run(&mut self) -> Result<()> {
        let block = self.block;
        if block.guard.is_some() {
            bail!("block {:#x} carries a self-modification guard; it stays tier-1", block.start);
        }
        if gex_abi::DISPATCH_SHIFT > gex_abi::INST_ALIGN_SHIFT {
            self.target_check();
        }
        for s in &block.stmts {
            self.stmt(s)?;
        }
        self.terminator()
    }

    // -- register cache (tier-1's `reg_local`, `flush`, `invalidate`) -------

    fn reg(&mut self, r: GuestReg) -> String {
        let name = reg_name(r);
        if self.func_regs.is_some() {
            return name;
        }
        if self.regs.contains_key(&r) {
            return name;
        }
        self.declared.insert(r, ());
        let off = reg_offset(r);
        let load = if r == GuestReg::CtxAddr {
            "(uint64_t)(uint32_t)ctx".to_string()
        } else if r.is_flag() {
            format!("GEX_C8({off}u)")
        } else {
            match r.ty() {
                Ty::I32 => format!("GEX_C32({off}u)"),
                Ty::I64 => format!("GEX_C64({off}u)"),
                Ty::V128 => format!("GEX_CV({off}u)"),
            }
        };
        self.line(format!("{name} = {load};"));
        self.regs.insert(r, false);
        name
    }

    fn write_reg(&mut self, r: GuestReg, val: &str) {
        self.declared.insert(r, ());
        self.line(format!("{} = {val};", reg_name(r)));
        self.regs.insert(r, true);
    }

    /// Function mode: the registers that may differ from the context here.
    pub(crate) fn set_dirty(&mut self, dirty: &BTreeSet<GuestReg>) {
        self.regs = dirty.iter().map(|r| (*r, true)).collect();
    }

    /// Pricing only (`GEX_CGEN_EXP=NOFLAGWB`): treat the flags as clean, so
    /// the next write-back skips them.
    pub(crate) fn forget_flags(&mut self) {
        self.regs.retain(|r, _| !r.is_flag());
    }

    /// A write-back on a conditional path: the stores happen, but the
    /// emitter keeps believing the registers are dirty.
    pub(crate) fn flush_keep(&mut self) {
        self.flush_inner(true, None);
    }

    /// Function mode: `flush_keep`, storing only registers in `live`.
    pub(crate) fn flush_keep_live(&mut self, live: RegSet) {
        self.flush_inner(true, Some(live));
    }

    pub(crate) fn flush(&mut self) {
        self.flush_inner(false, None);
    }

    /// Function mode: `flush`, storing only registers in `live`. The others
    /// are forgotten (treated as clean from here on): nothing reads them
    /// before they are written again.
    pub(crate) fn flush_live(&mut self, live: RegSet) {
        self.flush_inner(false, Some(live));
    }

    fn flush_inner(&mut self, keep_dirty: bool, live: Option<RegSet>) {
        if let Some(fr) = &self.func_regs {
            // Clean-spill elision: only what may differ from the context on
            // some path to here (`func::dirty_in`); otherwise everything the
            // function writes anywhere.
            let mut w: Vec<GuestReg> = if fr.clean_spills {
                self.regs.iter().filter(|(_, d)| **d).map(|(r, _)| *r).collect()
            } else {
                fr.written.iter().copied().collect()
            };
            if let Some(l) = live {
                w.retain(|r| l.contains(*r));
            }
            let loaded = fr.used.clone();
            self.store_regs(&w, &loaded);
            if !keep_dirty {
                self.regs.clear();
            }
            return;
        }
        let mut dirty: Vec<GuestReg> = self
            .regs
            .iter()
            .filter(|(_, d)| **d)
            .map(|(r, _)| *r)
            .collect();
        dirty.sort_by_key(|r| reg_offset(*r));
        let flags = [GuestReg::N, GuestReg::Z, GuestReg::C, GuestReg::V];
        let packed = flags.iter().all(|r| self.regs.contains_key(r))
            && flags.iter().any(|r| self.regs.get(r) == Some(&true));
        if packed {
            self.line(format!(
                "GEX_C32({}u) = f_n | (f_z << 8) | (f_c << 16) | (f_v << 24);",
                ctx::NZCV
            ));
            if !keep_dirty {
                for r in flags {
                    self.regs.insert(r, false);
                }
            }
        }
        let pa = [GuestReg::P, GuestReg::A];
        let pa_packed = pa.iter().all(|r| self.regs.contains_key(r))
            && pa.iter().any(|r| self.regs.get(r) == Some(&true));
        if pa_packed {
            self.line(format!("GEX_C16({}u) = (uint16_t)(f_p | (f_a << 8));", ctx::PF));
            if !keep_dirty {
                for r in pa {
                    self.regs.insert(r, false);
                }
            }
        }
        for r in dirty {
            if packed && flags.contains(&r) {
                continue;
            }
            if pa_packed && pa.contains(&r) {
                continue;
            }
            let off = reg_offset(r);
            let name = reg_name(r);
            if r.is_flag() {
                self.line(format!("GEX_C8({off}u) = (uint8_t){name};"));
            } else {
                match r.ty() {
                    Ty::I32 => self.line(format!("GEX_C32({off}u) = {name};")),
                    Ty::I64 => self.line(format!("GEX_C64({off}u) = {name};")),
                    Ty::V128 => self.line(format!("GEX_CV_SET({off}u, {name});")),
                }
            }
            if !keep_dirty {
                self.regs.insert(r, false);
            }
        }
    }

    /// Function mode: `invalidate`, except for registers known to hold what
    /// their locals already hold (a summarised callee's preserved set).
    pub(crate) fn invalidate_except(&mut self, keep: RegSet) {
        if let Some(fr) = &self.func_regs {
            let u: Vec<GuestReg> = fr.used.iter().copied().filter(|r| !keep.contains(*r)).collect();
            self.load_regs(&u);
            self.regs.clear();
            return;
        }
        self.regs.clear();
    }

    pub(crate) fn invalidate(&mut self) {
        if let Some(fr) = &self.func_regs {
            let u: Vec<GuestReg> = fr.used.iter().copied().collect();
            self.load_regs(&u);
            self.regs.clear();
            return;
        }
        self.regs.clear();
    }

    /// Function mode: load `regs` from the context into their locals.
    pub(crate) fn load_regs(&mut self, regs: &[GuestReg]) {
        for &r in regs {
            let off = reg_offset(r);
            let name = reg_name(r);
            let load = if r == GuestReg::CtxAddr {
                "(uint64_t)(uint32_t)ctx".to_string()
            } else if r.is_flag() {
                format!("GEX_C8({off}u)")
            } else {
                match r.ty() {
                    Ty::I32 => format!("GEX_C32({off}u)"),
                    Ty::I64 => format!("GEX_C64({off}u)"),
                    Ty::V128 => format!("GEX_CV({off}u)"),
                }
            };
            self.line(format!("{name} = {load};"));
        }
    }

    /// Function mode: store `regs` from their locals, packing the flags the
    /// way tier-1 does when all four are loaded.
    fn store_regs(&mut self, regs: &[GuestReg], loaded: &BTreeSet<GuestReg>) {
        // `GEX_CGEN_BALLAST=N`: N more stores at every write-back, into a slot
        // nothing reads, to price the write-back traffic before optimising it
        // (the OPTIMIZATIONS-MISSING.md method: a store cannot be deleted).
        for i in 0..ballast() {
            self.line(format!(
                "*(volatile uint64_t *)(GEX_CTXP(ctx) + {}u) = {i}u;",
                ctx::BALLAST
            ));
        }
        let mut regs: Vec<GuestReg> = regs.to_vec();
        regs.sort_by_key(|r| reg_offset(*r));
        let flags = [GuestReg::N, GuestReg::Z, GuestReg::C, GuestReg::V];
        let packed = flags.iter().all(|r| loaded.contains(r)) && flags.iter().any(|r| regs.contains(r));
        if packed {
            self.line(format!(
                "GEX_C32({}u) = f_n | (f_z << 8) | (f_c << 16) | (f_v << 24);",
                ctx::NZCV
            ));
        }
        // `GEX_CGEN_MUTATE_NOWB=<n>`: never write x<n> back. A deliberate
        // miscompilation, to check that the lockstep check catches one.
        let skip = std::env::var("GEX_CGEN_MUTATE_NOWB")
            .ok()
            .and_then(|v| v.parse::<u8>().ok())
            .map(GuestReg::X);
        for r in regs {
            if r == GuestReg::CtxAddr || (packed && flags.contains(&r)) || Some(r) == skip {
                continue;
            }
            let off = reg_offset(r);
            let name = reg_name(r);
            if r.is_flag() {
                self.line(format!("GEX_C8({off}u) = (uint8_t){name};"));
            } else {
                match r.ty() {
                    Ty::I32 => self.line(format!("GEX_C32({off}u) = {name};")),
                    Ty::I64 => self.line(format!("GEX_C64({off}u) = {name};")),
                    Ty::V128 => self.line(format!("GEX_CV_SET({off}u, {name});")),
                }
            }
        }
    }

    pub(crate) fn store_pc(&mut self, pc: &str) {
        self.line(format!("GEX_C64({}u) = {pc};", ctx::PC));
    }

    // -- addressing (tier-1's `emit_address`; see docs/function-tier.md §3) --

    /// A `char *` expression for a scalar or vector access at `addr + offset`.
    fn address(&self, addr: Value, offset: i64) -> String {
        if let Some(c) = self.value_const[addr.0 as usize] {
            let ea = c.wrapping_add(offset as u64);
            return if ea < GUEST_LIMIT {
                format!("GEX_P(0x{ea:x}u)")
            } else {
                "GEX_P(GEX_POISON_ADDR)".into()
            };
        }
        let a = self.v(addr);
        let max_static: i64 = if gex_abi::X86 { 0xffff } else { u32::MAX as i64 };
        if no_guard() {
            if (0..=max_static).contains(&offset) {
                return format!("(GEX_P((uint32_t){a}) + {offset}u)");
            }
            return format!("GEX_P((uint32_t)({a} + (uint64_t){offset}ll))");
        }
        if (0..=0xffff).contains(&offset) {
            if offset == 0 {
                format!("gex_gb({a})")
            } else {
                format!("(gex_gb({a}) + {offset}u)")
            }
        } else if (0..=max_static).contains(&offset) {
            format!("gex_g33({a}, {offset}ull)")
        } else {
            format!("GEX_P(gex_g({a} + (uint64_t){offset}ll))")
        }
    }

    /// The guarded 32-bit address of a bulk operation's operand (no offset).
    fn bulk_address(&self, v: &str) -> String {
        if no_guard() {
            format!("(uint32_t)({v})")
        } else {
            format!("gex_g({v})")
        }
    }

    // -- statements -------------------------------------------------------

    pub(crate) fn stmt(&mut self, s: &Stmt) -> Result<()> {
        match s {
            Stmt::Def { dst, expr } => self.def(*dst, expr)?,
            Stmt::WriteReg { reg, val } => {
                let v = self.v(*val);
                self.write_reg(*reg, &v);
            }
            Stmt::Store {
                addr,
                offset,
                val,
                sz,
                order,
            } => {
                let p = self.address(*addr, *offset);
                let v = self.v(*val);
                let (ut, at) = match sz {
                    MemSz::B1 => ("gex_u8", "gex_a8"),
                    MemSz::B2 => ("gex_u16", "gex_a16"),
                    MemSz::B4 => ("gex_u32", "gex_a32"),
                    MemSz::B8 => ("gex_u64", "gex_a64"),
                };
                let nt = match sz {
                    MemSz::B1 => "uint8_t",
                    MemSz::B2 => "uint16_t",
                    MemSz::B4 => "uint32_t",
                    MemSz::B8 => "uint64_t",
                };
                if *order == Order::SeqCst {
                    self.line(format!(
                        "__atomic_store_n(({at} *)({p}), ({nt}){v}, __ATOMIC_SEQ_CST);"
                    ));
                } else {
                    self.line(format!("*({ut} *)({p}) = ({nt}){v};"));
                }
            }
            Stmt::Trap { cond, pc, code } => {
                // tier-1's lowering: only the faulting path writes back
                // (everything, flags too: the host raises a signal from this
                // state), so the registers stay dirty for the path that
                // carries on.
                let c = self.v(*cond);
                self.line(format!("if (__builtin_expect({c} != 0, 0)) {{"));
                self.flush_keep();
                self.store_pc(&format!("0x{pc:x}ull"));
                self.line(format!("  gex_breakpoint(ctx, 0x{pc:x}ll, (int32_t){code});"));
                self.line("  __builtin_unreachable();");
                self.line("}");
            }
            Stmt::HostCall { func, args } => {
                let call = self.host_call_expr(*func, args)?;
                if func.clobbers_guest_state() {
                    self.flush();
                }
                self.line(format!("{call};"));
                if func.clobbers_guest_state() {
                    self.invalidate();
                }
            }
            Stmt::VStore {
                addr,
                offset,
                val,
                bytes,
            } => {
                let p = self.address(*addr, *offset);
                let v = self.v(*val);
                match bytes {
                    16 => self.line(format!("wasm_v128_store({p}, {v});")),
                    1 => self.line(format!("wasm_v128_store8_lane({p}, {v}, 0);")),
                    2 => self.line(format!("wasm_v128_store16_lane({p}, {v}, 0);")),
                    4 => self.line(format!("wasm_v128_store32_lane({p}, {v}, 0);")),
                    8 => self.line(format!("wasm_v128_store64_lane({p}, {v}, 0);")),
                    b => bail!("vector store of {b} bytes"),
                }
            }
            Stmt::MemCopy { dst, src, len } => {
                let d = self.bulk_address(&self.v(*dst));
                let s = self.bulk_address(&self.v(*src));
                let n = self.v(*len);
                self.line("{");
                self.line(format!("  uint32_t d = {d}, s = {s}; uint64_t n = {n}, room;"));
                self.clamp("d");
                self.clamp("s");
                self.line("  __builtin_memmove(GEX_P(d), GEX_P(s), (uint32_t)n);");
                self.line("}");
            }
            Stmt::MemFill { dst, val, len } => {
                let d = self.bulk_address(&self.v(*dst));
                let v = self.v(*val);
                let n = self.v(*len);
                self.line("{");
                self.line(format!("  uint32_t d = {d}; uint64_t n = {n}, room;"));
                self.clamp("d");
                self.line(format!("  __builtin_memset(GEX_P(d), (int)(uint32_t){v}, (uint32_t)n);"));
                self.line("}");
            }
            Stmt::MemCmp { dst, a, b, len } => {
                self.value_ty[dst.0 as usize] = Ty::I32;
                let (a, b, n, d) = (self.v(*a), self.v(*b), self.v(*len), self.v(*dst));
                let ga = self.bulk_address(&format!("{a} + i"));
                let gb = self.bulk_address(&format!("{b} + i"));
                self.line(format!("uint32_t {d} = 0;"));
                self.line(format!("for (uint64_t i = 0; i < {n}; i++) {{"));
                self.line(format!(
                    "  {d} = (uint32_t)*(gex_u8 *)GEX_P({ga}) - (uint32_t)*(gex_u8 *)GEX_P({gb});"
                ));
                self.line(format!("  if ({d}) break;"));
                self.line("}");
            }
            Stmt::Fence => self.line("__atomic_thread_fence(__ATOMIC_SEQ_CST);"),
            Stmt::StrOp { .. } => self.strop(s)?,
        }
        Ok(())
    }

    /// `tier1::clamp_len` for one base: `n = min(n, GUEST_LIMIT - base)`.
    fn clamp(&mut self, base: &str) {
        self.line(format!(
            "  room = 0x{GUEST_LIMIT:x}ull - (uint64_t){base}; n = n < room ? n : room;"
        ));
    }

    /// `tier1::emit_strop`, the same loop and fast paths.
    fn strop(&mut self, s: &Stmt) -> Result<()> {
        let Stmt::StrOp {
            kind,
            sz,
            until,
            count,
            dst,
            src,
            val,
            df,
            out_count,
            out_dst,
            out_src,
            out_a,
            out_b,
        } = s
        else {
            unreachable!()
        };
        let bytes = sz.bytes() as u64;
        let (count, dst, src, val, df) =
            (self.v(*count), self.v(*dst), self.v(*src), self.v(*val), self.v(*df));
        let ut = match bytes {
            1 => "gex_u8",
            2 => "gex_u16",
            4 => "gex_u32",
            _ => "gex_u64",
        };
        let mask: u64 = if bytes < 8 { (1u64 << (bytes * 8)) - 1 } else { u64::MAX };
        let id = self.fresh();
        for o in [out_count, out_dst, out_src, out_a, out_b] {
            self.value_ty[o.0 as usize] = Ty::I64;
        }
        let (oc, od, os, oa, ob) = (
            self.v(*out_count),
            self.v(*out_dst),
            self.v(*out_src),
            self.v(*out_a),
            self.v(*out_b),
        );
        self.line(format!("uint64_t {oc}, {od}, {os}, {oa}, {ob};"));
        self.line("{");
        self.line(format!(
            "  uint64_t lc = {count}, ld = {dst}, ls = {src}, lv = {val}, la = 0, lb = 0;"
        ));
        self.line(format!(
            "  uint64_t lstep = {df} ? (uint64_t)-{bytes}ll : {bytes}ull;"
        ));
        let g = |e: &str| if no_guard() { format!("(uint32_t)({e})") } else { format!("gex_g({e})") };
        if matches!(kind, StrKind::Movs | StrKind::Stos) {
            self.line(format!("  if (!{df} && lc != 0) {{"));
            match kind {
                StrKind::Movs => {
                    self.line(format!(
                        "    if ((ld <= ls) | (ld >= ls + lc * {bytes}ull)) {{"
                    ));
                    self.line(format!(
                        "      uint32_t d = {}, s = {}; uint64_t n = lc * {bytes}ull, room;",
                        g("ld"),
                        g("ls")
                    ));
                    self.clamp("  d");
                    self.clamp("  s");
                    self.line("      __builtin_memmove(GEX_P(d), GEX_P(s), (uint32_t)n);");
                }
                _ => {
                    if bytes > 1 {
                        let rep: u64 = match bytes {
                            2 => 0x0101,
                            4 => 0x0101_0101,
                            _ => 0x0101_0101_0101_0101,
                        };
                        self.line(format!(
                            "    if ((lv & 0x{mask:x}ull) == (lv & 0xff) * 0x{rep:x}ull) {{"
                        ));
                    } else {
                        self.line("    if (1) {");
                    }
                    self.line(format!(
                        "      uint32_t d = {}; uint64_t n = lc * {bytes}ull, room;",
                        g("ld")
                    ));
                    self.clamp("  d");
                    self.line("      __builtin_memset(GEX_P(d), (int)(uint32_t)lv, (uint32_t)n);");
                }
            }
            self.line(format!(
                "      ld += lc * {bytes}ull; ls += lc * {bytes}ull; lc = 0; goto {id};"
            ));
            self.line("    }");
            self.line("  }");
        }
        self.line("  while (lc != 0) {");
        match kind {
            StrKind::Movs => {
                self.line(format!("    la = *({ut} *)GEX_P({});", g("ls")));
                self.line(format!("    *({ut} *)GEX_P({}) = ({ut})la;", g("ld")));
            }
            StrKind::Stos => {
                self.line(format!("    *({ut} *)GEX_P({}) = ({ut})lv;", g("ld")));
            }
            StrKind::Scas => {
                self.line(format!("    la = lv & 0x{mask:x}ull;"));
                self.line(format!("    lb = *({ut} *)GEX_P({});", g("ld")));
            }
            StrKind::Cmps => {
                self.line(format!("    la = *({ut} *)GEX_P({});", g("ls")));
                self.line(format!("    lb = *({ut} *)GEX_P({});", g("ld")));
            }
        }
        self.line("    ld += lstep; ls += lstep; lc -= 1;");
        match until {
            StrUntil::Count => {}
            StrUntil::NotEqual => self.line("    if (la != lb) break;"),
            StrUntil::Equal => self.line("    if (la == lb) break;"),
        }
        self.line("  }");
        self.line(format!("  {id}:;"));
        self.line(format!("  {oc} = lc; {od} = ld; {os} = ls; {oa} = la; {ob} = lb;"));
        self.line("}");
        Ok(())
    }

    // -- expressions --------------------------------------------------------

    fn host_call_expr(&self, func: HostFunc, args: &[Value]) -> Result<String> {
        let name = match func {
            HostFunc::Syscall => "gex_syscall",
            HostFunc::Illegal => "gex_illegal",
            HostFunc::Breakpoint => "gex_breakpoint",
            HostFunc::SysRegRead => "gex_sysreg_read",
            HostFunc::SysRegWrite => "gex_sysreg_write",
            HostFunc::CacheOp => "gex_cache_op",
            HostFunc::Safepoint => "gex_safepoint",
            HostFunc::Fma => "gex_fma",
        };
        let want = match func {
            HostFunc::Syscall | HostFunc::Safepoint => 1,
            HostFunc::Illegal | HostFunc::Breakpoint | HostFunc::SysRegRead => 2,
            HostFunc::SysRegWrite | HostFunc::CacheOp => 3,
            HostFunc::Fma => 4,
        };
        if args.len() != want {
            bail!("{func:?} with {} arguments, the import takes {want}", args.len());
        }
        let mut s = format!("{name}(ctx");
        for a in args {
            let _ = write!(s, ", {}", self.v(*a));
        }
        s.push(')');
        Ok(s)
    }

    fn def(&mut self, dst: Value, expr: &Expr) -> Result<()> {
        let ty = expr.ty();
        let idx = dst.0 as usize;
        self.value_ty[idx] = ty;
        let d = self.v(dst);
        let rhs: String = match expr {
            Expr::ConstI32(c) => {
                self.value_const[idx] = Some(*c as u64);
                format!("0x{c:x}u")
            }
            Expr::ConstI64(c) => {
                self.value_const[idx] = Some(*c);
                format!("0x{c:x}ull")
            }
            Expr::ReadReg(r) => self.reg(*r),
            Expr::Bin { op, ty, a, b } => {
                let k = |v: &Value| self.value_const[v.0 as usize];
                self.value_half[idx] = *ty == Ty::I64
                    && match op {
                        BinOp::And => k(a) == Some(0xffff_ffff) || k(b) == Some(0xffff_ffff),
                        BinOp::ShrU => k(b).is_some_and(|s| (32..64).contains(&s)),
                        _ => false,
                    };
                self.bin(*op, *ty, *a, *b)
            }
            Expr::Un { op, ty, a } => {
                let a = self.v(*a);
                let w = *ty == Ty::I64;
                match op {
                    UnOp::Not => format!("~{a}"),
                    UnOp::Neg => format!("({})0 - {a}", cty(*ty)),
                    UnOp::Clz => format!(
                        "({})__builtin_clzg({a}, {})",
                        cty(*ty),
                        if w { 64 } else { 32 }
                    ),
                    UnOp::Ctz => format!(
                        "({})__builtin_ctzg({a}, {})",
                        cty(*ty),
                        if w { 64 } else { 32 }
                    ),
                    UnOp::Popcnt => format!("({})__builtin_popcountg({a})", cty(*ty)),
                }
            }
            Expr::Cmp { op, ty, a, b } => {
                let (a, b) = (self.v(*a), self.v(*b));
                let st = if *ty == Ty::I32 { "int32_t" } else { "int64_t" };
                let (sa, sb) = (format!("({st}){a}"), format!("({st}){b}"));
                let e = match op {
                    CmpOp::Eq => format!("{a} == {b}"),
                    CmpOp::Ne => format!("{a} != {b}"),
                    CmpOp::LtU => format!("{a} < {b}"),
                    CmpOp::LeU => format!("{a} <= {b}"),
                    CmpOp::GtU => format!("{a} > {b}"),
                    CmpOp::GeU => format!("{a} >= {b}"),
                    CmpOp::LtS => format!("{sa} < {sb}"),
                    CmpOp::LeS => format!("{sa} <= {sb}"),
                    CmpOp::GtS => format!("{sa} > {sb}"),
                    CmpOp::GeS => format!("{sa} >= {sb}"),
                };
                format!("(uint32_t)({e})")
            }
            Expr::Select { cond, a, b, .. } => {
                format!("{} ? {} : {}", self.v(*cond), self.v(*a), self.v(*b))
            }
            Expr::Wrap(a) => format!("(uint32_t){}", self.v(*a)),
            Expr::Extend { a, signed } => {
                if *signed {
                    format!("(uint64_t)(int64_t)(int32_t){}", self.v(*a))
                } else {
                    format!("(uint64_t){}", self.v(*a))
                }
            }
            Expr::ExtendBits {
                a,
                ty,
                bits,
                signed,
            } => {
                let a = self.v(*a);
                let width = ty.bits();
                if *signed {
                    let sh = width - bits;
                    let (u, s) = if *ty == Ty::I32 {
                        ("uint32_t", "int32_t")
                    } else {
                        ("uint64_t", "int64_t")
                    };
                    if sh == 0 {
                        a
                    } else {
                        format!("({u})(({s})({a} << {sh}) >> {sh})")
                    }
                } else {
                    let mask = if *bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
                    format!("{a} & 0x{mask:x}ull")
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
                let p = self.address(*addr, *offset);
                let t = cty(*ty);
                if *order == Order::SeqCst {
                    // Atomic loads have no signed form (tier-1 ignores `signed`
                    // here too: the architecture has no signed load-acquire).
                    let at = match sz {
                        MemSz::B1 => "gex_a8",
                        MemSz::B2 => "gex_a16",
                        MemSz::B4 => "gex_a32",
                        MemSz::B8 => "gex_a64",
                    };
                    format!("({t})__atomic_load_n(({at} *)({p}), __ATOMIC_SEQ_CST)")
                } else {
                    let vt = match (sz, signed) {
                        (MemSz::B1, false) => "gex_u8",
                        (MemSz::B1, true) => "gex_i8",
                        (MemSz::B2, false) => "gex_u16",
                        (MemSz::B2, true) => "gex_i16",
                        (MemSz::B4, false) => "gex_u32",
                        (MemSz::B4, true) => "gex_i32",
                        (MemSz::B8, _) => "gex_u64",
                    };
                    if *signed && *ty == Ty::I64 {
                        format!("(uint64_t)(int64_t)*({vt} *)({p})")
                    } else if *signed {
                        format!("(uint32_t)(int32_t)*({vt} *)({p})")
                    } else {
                        format!("({t})*({vt} *)({p})")
                    }
                }
            }
            Expr::Rmw {
                op,
                addr,
                val,
                sz,
                ty,
            } => {
                let p = self.address(*addr, 0);
                let (at, nt) = atomic_types(*sz);
                let v = self.v(*val);
                let f = match op {
                    RmwOp::Add => "__atomic_fetch_add",
                    RmwOp::Sub => "__atomic_fetch_sub",
                    RmwOp::And => "__atomic_fetch_and",
                    RmwOp::Or => "__atomic_fetch_or",
                    RmwOp::Xor => "__atomic_fetch_xor",
                    RmwOp::Xchg => "__atomic_exchange_n",
                };
                format!("({}){f}(({at} *)({p}), ({nt}){v}, __ATOMIC_SEQ_CST)", cty(*ty))
            }
            Expr::CmpXchg {
                addr,
                expected,
                replacement,
                sz,
                ty,
            } => {
                let p = self.address(*addr, 0);
                let (at, nt) = atomic_types(*sz);
                format!(
                    "({})GEX_CAS({at}, {p}, ({nt}){}, ({nt}){})",
                    cty(*ty),
                    self.v(*expected),
                    self.v(*replacement)
                )
            }
            Expr::HostCall { func, args, ty } => {
                if *func == HostFunc::Fma && args.len() == 4 {
                    if let Some(dbl) = self.value_const[args[3].0 as usize] {
                        let (a, b, c) = (self.v(args[0]), self.v(args[1]), self.v(args[2]));
                        let e = if dbl != 0 {
                            format!("gex_fma64(ctx, {a}, {b}, {c}, 1)")
                        } else {
                            format!("gex_fmaf({a}, {b}, {c})")
                        };
                        self.line(format!("const {} {d} = ({}){e};", cty(*ty), cty(*ty)));
                        return Ok(());
                    }
                }
                let call = self.host_call_expr(*func, args)?;
                if func.clobbers_guest_state() {
                    self.flush();
                }
                self.line(format!("const {} {d} = ({}){call};", cty(*ty), cty(*ty)));
                if func.clobbers_guest_state() {
                    self.invalidate();
                }
                return Ok(());
            }
            Expr::VLoad {
                addr,
                offset,
                bytes,
            } => {
                let p = self.address(*addr, *offset);
                match bytes {
                    16 => format!("wasm_v128_load({p})"),
                    8 => format!("wasm_v128_load64_zero({p})"),
                    4 => format!("wasm_v128_load32_zero({p})"),
                    1 => format!("wasm_v128_load8_lane({p}, wasm_i64x2_const(0, 0), 0)"),
                    2 => format!("wasm_v128_load16_lane({p}, wasm_i64x2_const(0, 0), 0)"),
                    b => bail!("vector load of {b} bytes"),
                }
            }
            Expr::VConst(b) => {
                let lo = u64::from_le_bytes(b[..8].try_into().unwrap());
                let hi = u64::from_le_bytes(b[8..].try_into().unwrap());
                format!("wasm_i64x2_const((int64_t)0x{lo:x}ull, (int64_t)0x{hi:x}ull)")
            }
            Expr::VSplat { a, lane_bytes } => {
                let a = self.v(*a);
                match lane_bytes {
                    1 => format!("wasm_i8x16_splat((int8_t){a})"),
                    2 => format!("wasm_i16x8_splat((int16_t){a})"),
                    4 => format!("wasm_i32x4_splat((int32_t){a})"),
                    _ => format!("wasm_i64x2_splat((int64_t){a})"),
                }
            }
            Expr::VBin { op, a, b } => {
                let f = match op {
                    VBinOp::And => "wasm_v128_and",
                    VBinOp::Or => "wasm_v128_or",
                    VBinOp::Xor => "wasm_v128_xor",
                    VBinOp::AndNot => "wasm_v128_andnot",
                };
                format!("{f}({}, {})", self.v(*a), self.v(*b))
            }
            Expr::VLaneBin { op, lane, a, b } => {
                format!("{}({}, {})", lane_bin(*op, *lane)?, self.v(*a), self.v(*b))
            }
            Expr::VLaneUn { op, lane, a } => {
                format!("{}({})", lane_un(*op, *lane)?, self.v(*a))
            }
            Expr::VShift {
                op,
                lane,
                a,
                amount,
            } => {
                let p = match lane {
                    VLane::I8 => ("i8x16", "u8x16"),
                    VLane::I16 => ("i16x8", "u16x8"),
                    VLane::I32 | VLane::F32 => ("i32x4", "u32x4"),
                    _ => ("i64x2", "u64x2"),
                };
                let f = match op {
                    VShiftOp::Shl => format!("wasm_{}_shl", p.0),
                    VShiftOp::ShrS => format!("wasm_{}_shr", p.0),
                    VShiftOp::ShrU => format!("wasm_{}_shr", p.1),
                };
                format!("{f}({}, (uint32_t){})", self.v(*a), self.v(*amount))
            }
            Expr::VShuffle { a, b, lanes } => {
                if lanes.iter().any(|l| *l >= 32) {
                    bail!("shuffle mask out of range: {lanes:?}");
                }
                let l: Vec<String> = lanes.iter().map(|x| x.to_string()).collect();
                format!(
                    "wasm_i8x16_shuffle({}, {}, {})",
                    self.v(*a),
                    self.v(*b),
                    l.join(", ")
                )
            }
            Expr::VSwizzle { a, idx } => {
                format!("wasm_i8x16_swizzle({}, {})", self.v(*a), self.v(*idx))
            }
            Expr::VBitSelect { mask, a, b } => format!(
                "wasm_v128_bitselect({}, {}, {})",
                self.v(*a),
                self.v(*b),
                self.v(*mask)
            ),
            Expr::VCvt { op, from, a, b } => {
                let f = lane_cvt(*op, *from)?;
                match b {
                    Some(b) => format!("{f}({}, {})", self.v(*a), self.v(*b)),
                    None => format!("{f}({})", self.v(*a)),
                }
            }
            Expr::VUn { op, a } => {
                let f = match op {
                    VUnOp::PopcntBytes => "wasm_i8x16_popcnt",
                    VUnOp::ExtAddPairwiseU8 => "wasm_u16x8_extadd_pairwise_u8x16",
                    VUnOp::ExtAddPairwiseU16 => "wasm_u32x4_extadd_pairwise_u16x8",
                    VUnOp::ExtAddPairwiseS8 => "wasm_i16x8_extadd_pairwise_i8x16",
                    VUnOp::ExtAddPairwiseS16 => "wasm_i32x4_extadd_pairwise_i16x8",
                };
                format!("{f}({})", self.v(*a))
            }
            Expr::VExtract {
                a,
                lane,
                lane_bytes,
                signed,
                ty,
            } => {
                let a = self.v(*a);
                // As an i32 first, the way the wasm instruction produces it.
                let as_i32 = match (lane_bytes, signed) {
                    (1, true) => format!("(uint32_t)(int32_t)wasm_i8x16_extract_lane({a}, {lane})"),
                    (1, false) => format!("(uint32_t)wasm_u8x16_extract_lane({a}, {lane})"),
                    (2, true) => format!("(uint32_t)(int32_t)wasm_i16x8_extract_lane({a}, {lane})"),
                    (2, false) => format!("(uint32_t)wasm_u16x8_extract_lane({a}, {lane})"),
                    (4, _) => format!("(uint32_t)wasm_i32x4_extract_lane({a}, {lane})"),
                    _ => format!("(uint64_t)wasm_i64x2_extract_lane({a}, {lane})"),
                };
                if *lane_bytes == 8 {
                    if *ty != Ty::I64 {
                        bail!("64-bit lane extracted into {ty:?}");
                    }
                    as_i32
                } else if *ty == Ty::I64 {
                    if *signed {
                        format!("(uint64_t)(int64_t)(int32_t)({as_i32})")
                    } else {
                        format!("(uint64_t)({as_i32})")
                    }
                } else {
                    as_i32
                }
            }
            Expr::VReplace {
                a,
                lane,
                lane_bytes,
                val,
            } => {
                let (a, v) = (self.v(*a), self.v(*val));
                match lane_bytes {
                    1 => format!("wasm_i8x16_replace_lane({a}, {lane}, (int8_t){v})"),
                    2 => format!("wasm_i16x8_replace_lane({a}, {lane}, (int16_t){v})"),
                    4 => format!("wasm_i32x4_replace_lane({a}, {lane}, (int32_t){v})"),
                    _ => format!("wasm_i64x2_replace_lane({a}, {lane}, (int64_t){v})"),
                }
            }
            Expr::VBitmask { a, lane } => {
                let f = match lane.bytes() {
                    1 => "wasm_i8x16_bitmask",
                    2 => "wasm_i16x8_bitmask",
                    4 => "wasm_i32x4_bitmask",
                    _ => "wasm_i64x2_bitmask",
                };
                format!("(uint32_t){f}({})", self.v(*a))
            }
            Expr::FpBin { op, double, a, b } => {
                let (fa, fb) = (self.fp(*a, *double), self.fp(*b, *double));
                let (sfx, back) = if *double { ("f64", "gex_b64") } else { ("f32", "gex_b32") };
                let e = match op {
                    FpBinOp::Add => format!("{fa} + {fb}"),
                    FpBinOp::Sub => format!("{fa} - {fb}"),
                    FpBinOp::Mul => format!("{fa} * {fb}"),
                    FpBinOp::Div => format!("{fa} / {fb}"),
                    FpBinOp::Min => format!("__builtin_wasm_min_{sfx}({fa}, {fb})"),
                    FpBinOp::Max => format!("__builtin_wasm_max_{sfx}({fa}, {fb})"),
                    FpBinOp::Copysign => {
                        if *double {
                            format!("__builtin_copysign({fa}, {fb})")
                        } else {
                            format!("__builtin_copysignf({fa}, {fb})")
                        }
                    }
                };
                format!("{back}({e})")
            }
            Expr::FpUn { op, double, a } => {
                let fa = self.fp(*a, *double);
                let (f, back) = if *double { ("", "gex_b64") } else { ("f", "gex_b32") };
                let e = match op {
                    FpUnOp::Abs => format!("__builtin_fabs{f}({fa})"),
                    FpUnOp::Neg => format!("-{fa}"),
                    FpUnOp::Sqrt => format!("__builtin_sqrt{f}({fa})"),
                    FpUnOp::RoundNearest => format!("__builtin_roundeven{f}({fa})"),
                    FpUnOp::Ceil => format!("__builtin_ceil{f}({fa})"),
                    FpUnOp::Floor => format!("__builtin_floor{f}({fa})"),
                    FpUnOp::Trunc => format!("__builtin_trunc{f}({fa})"),
                };
                format!("{back}({e})")
            }
            Expr::FpCmp { op, double, a, b } => {
                let (fa, fb) = (self.fp(*a, *double), self.fp(*b, *double));
                let o = match op {
                    FpCmpOp::Eq => "==",
                    FpCmpOp::Ne => "!=",
                    FpCmpOp::Lt => "<",
                    FpCmpOp::Le => "<=",
                    FpCmpOp::Gt => ">",
                    FpCmpOp::Ge => ">=",
                };
                format!("(uint32_t)({fa} {o} {fb})")
            }
            Expr::FpCvt {
                kind,
                src_wide,
                dst_wide,
                a,
            } => self.fp_convert(*kind, *src_wide, *dst_wide, *a),
        };
        self.line(format!("const {} {d} = {rhs};", cty(ty)));
        Ok(())
    }

    /// A value's bit pattern as a C float of the given precision.
    fn fp(&self, v: Value, double: bool) -> String {
        if double {
            format!("gex_f64({})", self.v(v))
        } else {
            format!("gex_f32((uint32_t){})", self.v(v))
        }
    }

    fn fp_convert(&self, kind: FpConvert, src_wide: bool, dst_wide: bool, a: Value) -> String {
        let av = self.v(a);
        match kind {
            FpConvert::FpToFp => {
                let f = self.fp(a, src_wide);
                if dst_wide {
                    format!("gex_b64((double){f})")
                } else {
                    format!("gex_b32((float){f})")
                }
            }
            FpConvert::IntToFp { signed } => {
                let src = match (src_wide, signed) {
                    (false, true) => format!("(int32_t){av}"),
                    (false, false) => format!("(uint32_t){av}"),
                    (true, true) => format!("(int64_t){av}"),
                    (true, false) => format!("(uint64_t){av}"),
                };
                if dst_wide {
                    format!("gex_b64((double){src})")
                } else {
                    format!("gex_b32((float){src})")
                }
            }
            FpConvert::FpToInt { signed } => {
                let f = self.fp(a, src_wide);
                let b = format!(
                    "__builtin_wasm_trunc_saturate_{}_i{}_f{}",
                    if signed { "s" } else { "u" },
                    if dst_wide { 64 } else { 32 },
                    if src_wide { 64 } else { 32 }
                );
                if dst_wide {
                    format!("(uint64_t){b}({f})")
                } else {
                    format!("(uint32_t){b}({f})")
                }
            }
        }
    }

    fn bin(&self, op: BinOp, ty: Ty, av: Value, bv: Value) -> String {
        let (a, b) = (self.v(av), self.v(bv));
        let w = ty == Ty::I64;
        let (u, s, m) = if w {
            ("uint64_t", "int64_t", 63)
        } else {
            ("uint32_t", "int32_t", 31)
        };
        match op {
            BinOp::Add => format!("{a} + {b}"),
            BinOp::Sub => format!("{a} - {b}"),
            // Four 32x32 products of the halves are how the lifter spells
            // `umulh`; LLVM recognises them and forms an i128 multiply, which
            // wasm32 lowers to `__multi3` returning through the shadow stack
            // (which generated code must never touch, §4 of the note). An empty
            // asm on one operand hides the idiom, at the cost of nothing else.
            BinOp::Mul
                if w && self.value_half[av.0 as usize] && self.value_half[bv.0 as usize] =>
            {
                format!("gex_opaque64({a}) * {b}")
            }
            BinOp::Mul => format!("{a} * {b}"),
            BinOp::And => format!("{a} & {b}"),
            BinOp::Or => format!("{a} | {b}"),
            BinOp::Xor => format!("{a} ^ {b}"),
            BinOp::Shl => format!("{a} << ({b} & {m})"),
            BinOp::ShrU => format!("{a} >> ({b} & {m})"),
            BinOp::ShrS => format!("({u})(({s}){a} >> ({b} & {m}))"),
            BinOp::Rotr => {
                if w {
                    format!("__builtin_rotateright64({a}, {b})")
                } else {
                    format!("__builtin_rotateright32({a}, {b})")
                }
            }
            BinOp::DivU => format!("{a} / {b}"),
            BinOp::RemU => format!("{a} % {b}"),
            BinOp::DivS => format!("({u})(({s}){a} / ({s}){b})"),
            BinOp::RemS => format!("({u})(({s}){a} % ({s}){b})"),
        }
    }

    // -- control flow (tier-1's `terminator`, `jump_to_constant`) -----------

    pub(crate) fn safepoint(&mut self, pc: &str) {
        let c = ctx::SAFEPOINT_COUNTER;
        self.line(format!(
            "{{ int32_t sc = (int32_t)GEX_C32({c}u) - 1; GEX_C32({c}u) = (uint32_t)sc; \
             if (sc <= 0) gex_safepoint(ctx, (int64_t)({pc})); }}"
        ));
    }

    /// Tail-call dispatch on `target` (tier-1's `emit_dispatch_tail_call`,
    /// whose stores this makes before handing over).
    pub(crate) fn dispatch(&mut self, target: &str) {
        self.line(format!("GEX_C64({}u) = {target};", ctx::LAST_TARGET));
        if gex_abi::X86 {
            self.line(format!(
                "GEX_C64({}u) = 0x{:x}ull;",
                ctx::LAST_SOURCE,
                self.block.start
            ));
        }
        self.line("GEX_MUSTTAIL return gexc_dispatch(ctx);");
    }

    fn jump(&mut self, target: u64) {
        if self.owned.contains(&target) {
            if gex_abi::DISPATCH_SHIFT > gex_abi::INST_ALIGN_SHIFT {
                self.line(format!("GEX_C64({}u) = 0x{target:x}ull;", ctx::LAST_TARGET));
            }
            self.called.insert(target);
            self.line(format!("GEX_MUSTTAIL return {}(ctx);", fn_name(target)));
            return;
        }
        self.store_pc(&format!("0x{target:x}ull"));
        self.dispatch(&format!("0x{target:x}ull"));
    }

    /// x86-64: a slot covers four bytes, so a block can be entered for a
    /// neighbour's address (tier-1's `emit_target_check`).
    pub(crate) fn target_check(&mut self) {
        let start = self.block.start;
        let lt = ctx::LAST_TARGET;
        self.line(format!("if (GEX_C64({lt}u) != 0x{start:x}ull) {{"));
        self.line(format!(
            "  gex_cache_op(ctx, 0x{start:x}ll, GEX_CACHE_OP_SLOT_CONFLICT, (int64_t)GEX_C64({lt}u));"
        ));
        self.line(format!("  (void)gex_dispatch_miss(ctx, (int64_t)GEX_C64({lt}u));"));
        self.line(format!("  uint64_t t0 = GEX_C64({lt}u);"));
        self.out.push_str("  ");
        self.dispatch("t0");
        self.line("}");
    }

    fn terminator(&mut self) -> Result<()> {
        let block = self.block;
        match block.term {
            Term::Goto { target } => {
                self.flush();
                if target <= block.start {
                    self.safepoint(&format!("0x{target:x}ull"));
                }
                self.jump(target);
            }
            Term::Branch {
                cond,
                taken,
                not_taken,
            } => {
                self.flush();
                let c = self.v(cond);
                self.line(format!("if ({c}) {{"));
                if taken <= block.start {
                    self.safepoint(&format!("0x{taken:x}ull"));
                }
                self.jump(taken);
                self.line("}");
                if not_taken <= block.start {
                    self.safepoint(&format!("0x{not_taken:x}ull"));
                }
                self.jump(not_taken);
            }
            Term::Indirect { target, .. } => {
                let t = self.v(target);
                self.line(format!("const uint64_t tgt = {t};"));
                self.store_pc("tgt");
                self.flush();
                if gex_abi::X86 {
                    self.safepoint("tgt");
                } else {
                    let pc = block.end - gex_abi::INST_ALIGN;
                    self.safepoint(&format!("0x{pc:x}ull"));
                }
                self.dispatch("tgt");
            }
            Term::Syscall { next } => {
                self.flush();
                self.store_pc(&format!("0x{next:x}ull"));
                let at = block.end - gex_abi::SYSCALL_INST_BYTES;
                self.line(format!("gex_syscall(ctx, 0x{at:x}ll);"));
                self.invalidate();
                self.jump(next);
            }
            Term::Illegal { raw, pc } => {
                self.flush();
                self.store_pc(&format!("0x{pc:x}ull"));
                self.line(format!("gex_illegal(ctx, 0x{pc:x}ll, (int32_t)0x{raw:x}u);"));
                self.line("__builtin_trap();");
            }
        }
        Ok(())
    }
}

fn atomic_types(sz: MemSz) -> (&'static str, &'static str) {
    match sz {
        MemSz::B1 => ("gex_a8", "uint8_t"),
        MemSz::B2 => ("gex_a16", "uint16_t"),
        MemSz::B4 => ("gex_a32", "uint32_t"),
        MemSz::B8 => ("gex_a64", "uint64_t"),
    }
}

fn lane_prefix(lane: VLane, unsigned: bool) -> &'static str {
    match (lane, unsigned) {
        (VLane::I8, false) => "i8x16",
        (VLane::I8, true) => "u8x16",
        (VLane::I16, false) => "i16x8",
        (VLane::I16, true) => "u16x8",
        (VLane::I32, false) => "i32x4",
        (VLane::I32, true) => "u32x4",
        (VLane::I64, false) => "i64x2",
        (VLane::I64, true) => "u64x2",
        (VLane::F32, _) => "f32x4",
        (VLane::F64, _) => "f64x2",
    }
}

/// The `wasm_simd128.h` intrinsic for a lane-wise binary operation, exactly
/// where `VLaneBinOp::wasm_opcode` has one (the test checks the opcodes).
pub(crate) fn lane_bin(op: VLaneBinOp, lane: VLane) -> Result<String> {
    use VLaneBinOp::*;
    if op.wasm_opcode(lane).is_none() {
        bail!("no wasm SIMD instruction for {op:?} on {lane:?}");
    }
    let unsigned = matches!(
        op,
        MinU | MaxU | AddSatU | SubSatU | AvgrU | LtU | GtU | LeU | GeU
    );
    let name = match op {
        Add => "add",
        Sub => "sub",
        Mul => "mul",
        Div => "div",
        MinS | MinU => "min",
        MaxS | MaxU => "max",
        AddSatS | AddSatU => "add_sat",
        SubSatS | SubSatU => "sub_sat",
        AvgrU => "avgr",
        Eq => "eq",
        Ne => "ne",
        LtS | LtU => "lt",
        GtS | GtU => "gt",
        LeS | LeU => "le",
        GeS | GeU => "ge",
    };
    Ok(format!("wasm_{}_{name}", lane_prefix(lane, unsigned)))
}

pub(crate) fn lane_un(op: VLaneUnOp, lane: VLane) -> Result<String> {
    use VLaneUnOp::*;
    if op.wasm_opcode(lane).is_none() {
        bail!("no wasm SIMD instruction for {op:?} on {lane:?}");
    }
    let name = match op {
        Neg => "neg",
        Abs => "abs",
        Sqrt => "sqrt",
        Ceil => "ceil",
        Floor => "floor",
        Trunc => "trunc",
        Nearest => "nearest",
    };
    Ok(format!("wasm_{}_{name}", lane_prefix(lane, false)))
}

/// `tier1::lane_cvt_op`, as intrinsics, including its fallthrough arms.
pub(crate) fn lane_cvt(op: VCvtOp, from: VLane) -> Result<&'static str> {
    use VCvtOp::*;
    Ok(match (op, from) {
        (FpToIntSat { signed: true }, VLane::F32) => "wasm_i32x4_trunc_sat_f32x4",
        (FpToIntSat { signed: false }, VLane::F32) => "wasm_u32x4_trunc_sat_f32x4",
        (FpToIntSat { signed: true }, VLane::F64) => "wasm_i32x4_trunc_sat_f64x2_zero",
        (FpToIntSat { signed: false }, VLane::F64) => "wasm_u32x4_trunc_sat_f64x2_zero",
        (IntToFp { signed: true }, VLane::I32) => "wasm_f32x4_convert_i32x4",
        (IntToFp { signed: false }, VLane::I32) => "wasm_f32x4_convert_u32x4",
        (IntToFp { .. }, VLane::I64) | (FpToIntSat { .. }, _) => {
            bail!("no wasm SIMD conversion for {op:?} from {from:?}")
        }
        (IntToFp { signed: true }, _) => "wasm_f64x2_convert_low_i32x4",
        (IntToFp { signed: false }, _) => "wasm_f64x2_convert_low_u32x4",
        (DemoteF64, _) => "wasm_f32x4_demote_f64x2_zero",
        (PromoteF32, _) => "wasm_f64x2_promote_low_f32x4",
        (ExtendLow { signed: true }, VLane::I8) => "wasm_i16x8_extend_low_i8x16",
        (ExtendHigh { signed: true }, VLane::I8) => "wasm_i16x8_extend_high_i8x16",
        (ExtendLow { signed: false }, VLane::I8) => "wasm_u16x8_extend_low_u8x16",
        (ExtendHigh { signed: false }, VLane::I8) => "wasm_u16x8_extend_high_u8x16",
        (ExtendLow { signed: true }, VLane::I16) => "wasm_i32x4_extend_low_i16x8",
        (ExtendHigh { signed: true }, VLane::I16) => "wasm_i32x4_extend_high_i16x8",
        (ExtendLow { signed: false }, VLane::I16) => "wasm_u32x4_extend_low_u16x8",
        (ExtendHigh { signed: false }, VLane::I16) => "wasm_u32x4_extend_high_u16x8",
        (ExtendLow { signed: true }, _) => "wasm_i64x2_extend_low_i32x4",
        (ExtendHigh { signed: true }, _) => "wasm_i64x2_extend_high_i32x4",
        (ExtendLow { signed: false }, _) => "wasm_u64x2_extend_low_u32x4",
        (ExtendHigh { signed: false }, _) => "wasm_u64x2_extend_high_u32x4",
        (NarrowSat { signed: true }, VLane::I16) => "wasm_i8x16_narrow_i16x8",
        (NarrowSat { signed: false }, VLane::I16) => "wasm_u8x16_narrow_i16x8",
        (NarrowSat { signed: true }, _) => "wasm_i16x8_narrow_i32x4",
        (NarrowSat { signed: false }, _) => "wasm_u16x8_narrow_i32x4",
    })
}

/// The intrinsic `lane_bin` names, for the opcode test.
pub fn lane_bin_name(op: VLaneBinOp, lane: VLane) -> Result<String> {
    lane_bin(op, lane)
}

/// The intrinsic `lane_un` names, for the opcode test.
pub fn lane_un_name(op: VLaneUnOp, lane: VLane) -> Result<String> {
    lane_un(op, lane)
}

/// Whether a statement is a host call that can change guest state, so the
/// function tier writes back before it and reloads after.
pub(crate) fn clobbers(s: &Stmt) -> bool {
    match s {
        Stmt::HostCall { func, .. } => func.clobbers_guest_state(),
        Stmt::Def {
            expr: Expr::HostCall { func, .. },
            ..
        } => func.clobbers_guest_state(),
        _ => false,
    }
}
