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
    pub const I16X8_EXTADD_PAIRWISE_I8X16_S: u32 = 0x7c;
    pub const I16X8_EXTADD_PAIRWISE_I8X16_U: u32 = 0x7d;
    pub const I32X4_EXTADD_PAIRWISE_I16X8_S: u32 = 0x7e;
    pub const I32X4_EXTADD_PAIRWISE_I16X8_U: u32 = 0x7f;
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
    /// Guest address -> the block's GIR, for growing a region around a loop.
    pub gir: &'a HashMap<u64, &'a Block>,
}

/// How many blocks may go into one region. One turns regions off.
fn region_limit() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("GEX_REGION")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8)
    })
}

/// Whether every block should check the guest's stack pointer on entry.
///
/// Off unless asked for: `GEX_CHECK_SP=1` natively, or `gex_set_check_sp(1)`
/// in the browser, where there is no environment to read. Costs a load, a
/// compare and a never-taken branch per block.
///
/// It exists because a corrupt `sp` is invisible until something dereferences
/// it, and by then the guest is deep inside an innocent function. #73 ends in
/// `stp x29, x30, [sp, #-32]!` -- a perfectly ordinary prologue, faulting on
/// its own first store because control reached it with `sp` already wild. The
/// address in the crash names the victim; this names the writer.
static CHECK_SP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Turn the stack-pointer check on or off for blocks compiled after this.
pub fn set_check_sp(on: bool) {
    CHECK_SP.store(on, std::sync::atomic::Ordering::Relaxed);
}

fn check_sp() -> bool {
    static ENV: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENV.get_or_init(|| std::env::var_os("GEX_CHECK_SP").is_some())
        || CHECK_SP.load(std::sync::atomic::Ordering::Relaxed)
}

/// Compile one GIR block into a wasm function body.
///
/// When the block heads a loop, the loop's other blocks come with it: see
/// [`Emitter::run_region`].
pub fn compile(block: &Block, cx: &Context<'_>) -> Vec<u8> {
    let mut e = Emitter::new(cx);
    if check_sp() {
        e.emit_sp_check(block.start);
    }
    match crate::region::grow(block.start, cx.gir, region_limit()) {
        Some(region) if region_limit() > 1 => {
            if std::env::var_os("GEX_REGION_DEBUG").is_some() {
                let members: Vec<String> = region.iter().map(|a| format!("{a:#x}")).collect();
                eprintln!("[region] {}", members.join(" "));
            }
            REGIONS.with(|c| {
                let (n, b) = c.get();
                c.set((n + 1, b + region.len()))
            });
            e.run_region(&region)
        }
        _ => e.run(block),
    }
    e.f.finish()
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
    /// Scratch for a directory entry, and then for the table it names.
    ///
    /// Only used when there is more than one dispatch table; with one, the
    /// entry is consumed on the stack and never needs a name.
    scratch_entry: u32,
    /// Scratch for the computed slot, held across the `br_table`.
    ///
    /// A value pushed outside a `block` cannot be consumed inside one, so the
    /// index has to be in a local before the selection starts.
    scratch_index: u32,
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
    /// Set while emitting a member of a region.
    region: Option<RegionState>,
    /// Which registers the region writes, and so has to store on the way out.
    /// The ones it only reads are already correct in the context.
    region_writes: std::collections::HashSet<GuestReg>,
}

/// What a region member needs to know to branch to its neighbours.
struct RegionState {
    /// Guest address -> arm of the dispatch table.
    index: HashMap<u64, u32>,
    /// The local holding which arm to run next.
    idx_local: u32,
    /// How many labels lie between here and the region's `loop`.
    depth: u32,
}

impl<'a> Emitter<'a> {
    fn new(cx: &'a Context<'a>) -> Emitter<'a> {
        let mut f = FuncBody::new(1);
        let scratch_i64 = f.add_local(ValType::I64);
        let scratch_i32 = f.add_local(ValType::I32);
        let scratch_page = f.add_local(ValType::I64);
        let scratch_entry = f.add_local(ValType::I32);
        let scratch_index = f.add_local(ValType::I32);
        Emitter {
            f,
            cx,
            ctx_local: 0,
            scratch_i64,
            scratch_i32,
            scratch_page,
            scratch_entry,
            scratch_index,
            value_local: Vec::new(),
            value_const: Vec::new(),
            value_ty: Vec::new(),
            free_i32: Vec::new(),
            free_i64: Vec::new(),
            free_v128: Vec::new(),
            regs: HashMap::new(),
            loop_depth: None,
            reenter_local: None,
            region: None,
            region_writes: Default::default(),
        }
    }

    /// Emit a whole loop as one function.
    ///
    /// The members become the arms of a `br_table` inside a `loop`, so an
    /// edge between them is a branch and an assignment rather than a tail
    /// call, and every guest register the loop touches is loaded once at the
    /// top and stays in a local until control leaves. That is the difference
    /// between a loop costing a context round-trip per edge and costing
    /// nothing per edge, and it is also what gives V8 one function worth
    /// optimising instead of half a dozen it will not bother with.
    ///
    /// Laid out as the usual nest: the table is in the innermost block, and
    /// breaking out of the *k*th lands on the *k*th body.
    ///
    /// # What makes this awkward
    ///
    /// Three things, all of which follow from the same fact: within a region
    /// the emitter walks the members in one order and the guest reaches them
    /// in another.
    ///
    /// Which registers are *dirty* is a fact about emission order, so every
    /// exit writes all of them rather than the dirty ones. The safepoint
    /// writes them all too, for the same reason -- but deliberately does not
    /// read them back afterwards, because the host only changes a register on
    /// a path that does not return here, and reading them back is what made a
    /// loop with a `continue` and a `break` in it never finish. And a region
    /// holds exactly one loop, through its entry, so the poll at that entry
    /// is on every cycle; polling at an inner back edge instead left a guest
    /// thread the browser preempted there resuming with another thread's
    /// registers.
    ///
    /// # What it is worth
    ///
    /// Only leaf loops qualify -- a call in the body ends the region, because
    /// the callee returns through the table -- so this fires on a few hundred
    /// loops in a large program: 195 in Lua, 638 in zstd, 1287 in SQLite. On
    /// nbench in the browser, against the same module built without it:
    ///
    /// ```text
    ///     ASSIGNMENT   +15%      FOURIER        +3%
    ///     NUMERIC SORT +13%      IDEA           -3%
    ///     STRING SORT  +10%      FP EMULATION   -7%
    ///     BITFIELD      +3%      geometric mean +5%
    /// ```
    ///
    /// Assignment and numeric sort are two of the kernels where gex was
    /// behind QEMU. zstd compressing at level 9 is 13% faster; at level 19,
    /// unchanged. Lua, matmul and a call microbenchmark are unchanged, which
    /// is what a transform reaching only leaf loops should look like.
    ///
    /// FP emulation reads as a loss above and is not one: nbench's kernel
    /// pulled out into a program of its own and measured the same way comes
    /// out at 1.06, 1.04 and 0.91 across three runs. It straddles even, and
    /// what the table shows is this machine's spread, which is about 8%.
    fn run_region(&mut self, region: &[u64]) {
        let n = region.len();
        let index: HashMap<u64, u32> = region
            .iter()
            .enumerate()
            .map(|(i, a)| (*a, i as u32))
            .collect();

        // Every register the loop touches starts in a local holding the
        // context's value -- including one the loop only ever writes, because
        // an exit taken on a path that has not reached the write yet still
        // stores the local, and an uninitialised one would put a zero in the
        // context. What the two sets are actually for is the way *out*: a
        // register the loop only reads is already correct in the context and
        // needs no store, and a region pays that store on every exit.
        let (mut reads, mut writes): (Vec<GuestReg>, Vec<GuestReg>) = (Vec::new(), Vec::new());
        for a in region {
            let (r, w) = registers_read_and_written(self.cx.gir[a]);
            for x in r {
                if !reads.contains(&x) {
                    reads.push(x);
                }
            }
            for x in w {
                if !writes.contains(&x) {
                    writes.push(x);
                }
            }
        }
        self.region_writes = writes.iter().copied().collect();
        let mut touched = reads;
        for w in writes {
            if !touched.contains(&w) {
                touched.push(w);
            }
        }
        touched.sort_by_key(|r| Self::reg_offset(*r));
        if std::env::var_os("GEX_REGION_DEBUG").is_some() {
            let insts: u64 = region
                .iter()
                .map(|a| (self.cx.gir[a].end - self.cx.gir[a].start) / 4)
                .sum();
            eprintln!(
                "[region] {} blocks, {} instructions, {} registers held",
                n,
                insts,
                touched.len()
            );
        }
        for r in touched {
            self.reg_local(r);
        }

        let idx = self.alloc(Ty::I32);
        self.f.i32_const(0);
        self.f.local_set(idx);
        let counter = self.alloc(Ty::I32);
        self.f.i32_const(loop_poll());
        self.f.local_set(counter);
        self.reenter_local = Some(counter);

        self.f.loop_void();
        for _ in 0..n {
            self.f.block_void();
        }
        self.f.local_get(idx);
        // Inside the innermost block, member k's label is n-1-k deep.
        let targets: Vec<u32> = (0..n).map(|k| (n - 1 - k) as u32).collect();
        self.f.br_table(&targets, targets[0]);
        self.f.end();

        // Bodies come out innermost first: the last member's body sits just
        // inside the second-innermost block.
        for k in (0..n).rev() {
            let block = self.cx.gir[&region[k]];
            // Depth from here back to the loop: k blocks still open.
            self.region = Some(RegionState {
                index: index.clone(),
                idx_local: idx,
                depth: k as u32,
            });
            self.run_member(block);
            self.region = None;
            if k > 0 {
                self.f.end();
            }
        }
        self.f.end(); // loop
                      // Every member either branches back to the table or leaves the
                      // function, so falling out of the loop cannot happen.
        self.f.unreachable();
    }

    /// Write every register the region holds back to the context.
    ///
    /// Not just the dirty ones. Which registers are dirty is a fact about the
    /// order the emitter walked the members in, and at run time control
    /// reaches them in whatever order the guest's branches take -- a register
    /// written by a member emitted later can already have been written by the
    /// time this exit is taken. Storing all of them is the only version of
    /// this that does not depend on emission order, and it costs stores only
    /// where control leaves the loop.
    fn flush_region_exit(&mut self) {
        let mut all: Vec<(GuestReg, u32)> = self
            .regs
            .iter()
            .filter(|(r, _)| self.region_writes.contains(*r))
            .map(|(r, c)| (*r, c.local))
            .collect();
        all.sort_by_key(|(r, _)| Self::reg_offset(*r));
        for (r, local) in all {
            note_traffic(r);
            self.f.local_get(self.ctx_local);
            self.f.local_get(local);
            match r.ty() {
                Ty::I32 => self.f.store(0x36, 2, Self::reg_offset(r)),
                Ty::I64 => self.f.store(0x37, 3, Self::reg_offset(r)),
                Ty::V128 => self.f.simd_mem(simd::V128_STORE, 4, Self::reg_offset(r)),
            }
        }
    }

    /// One member of a region: its body, then its terminator translated into
    /// region terms.
    fn run_member(&mut self, block: &Block) {
        let n = block.n_values as usize;
        self.value_local = vec![None; n];
        self.value_const = vec![None; n];
        self.value_ty = vec![Ty::I64; n];
        let last_use = compute_last_use(block);
        for (i, stmt) in block.stmts.iter().enumerate() {
            self.stmt(stmt, block);
            for (v, &lu) in last_use.iter().enumerate() {
                if lu == Some(i) {
                    self.release(Value(v as u32));
                }
            }
        }
        self.region_terminator(block);
    }

    /// Go to `target`: a branch back to the table when it is in the region,
    /// and the ordinary flush-and-tail-call when it is not.
    fn region_goto(&mut self, target: u64, from: u64) {
        let st = self.region.as_ref().expect("not in a region");
        match st.index.get(&target).copied() {
            Some(k) => {
                let (idx_local, depth) = (st.idx_local, st.depth);
                // The region's only back edge goes to its entry, by
                // construction, so this is the poll for the whole loop.
                if k == 0 && target <= from {
                    self.emit_safepoint(target);
                }
                self.f.i32_const(k as i32);
                self.f.local_set(idx_local);
                self.f.br(depth);
            }
            None => {
                self.flush_region_exit();
                if target <= from {
                    self.emit_safepoint(target);
                }
                self.jump_to_constant(target);
            }
        }
    }

    fn region_terminator(&mut self, block: &Block) {
        match block.term {
            Term::Goto { target } => self.region_goto(target, block.start),
            Term::Branch {
                cond,
                taken,
                not_taken,
            } => {
                self.push(cond);
                self.f.if_void();
                // One more block is open inside the `if`.
                if let Some(st) = self.region.as_mut() {
                    st.depth += 1;
                }
                self.region_goto(taken, block.start);
                if let Some(st) = self.region.as_mut() {
                    st.depth -= 1;
                }
                self.f.end();
                self.region_goto(not_taken, block.start);
            }
            // `region::grow` admits nothing else.
            _ => unreachable!("region member with a non-branch terminator"),
        }
    }

    fn run(&mut self, block: &Block) {
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
        self.terminator(block);
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
    /// Trap into the host if the guest's stack pointer is not a stack address.
    ///
    /// Emitted before anything else in the body, so the value tested is what
    /// the *previous* block left behind rather than anything this one did.
    ///
    ///     local.get ctx
    ///     i64.load  offset=SP
    ///     i64.const STACK_TOP
    ///     i64.ge_u                 ; unsigned: a negative sp is a huge one
    ///     if
    ///       local.get ctx
    ///       i64.const <this block's guest address>
    ///       i32.const BRK_BAD_SP
    ///       call      breakpoint
    ///     end
    ///
    /// One bound, not two. Above `STACK_TOP` is the shim's own world and no
    /// guest `sp` is ever there, which catches a wild value however it went
    /// wrong; a lower bound would have to know where this thread's stack was
    /// mapped, and the emitter does not and should not.
    ///
    /// The register cache is untouched on purpose -- this reads the context
    /// directly rather than through `reg_local`, so it cannot make the body
    /// that follows believe `sp` is already in a local.
    fn emit_sp_check(&mut self, pc: u64) {
        self.f.local_get(self.ctx_local);
        self.f.load(0x29, 3, ctx::SP as u32);
        self.f.i64_const(gex_abi::STACK_TOP as i64);
        self.f.byte(0x5a); // i64.ge_u
        self.f.if_void();
        self.f.local_get(self.ctx_local);
        self.f.i64_const(pc as i64);
        self.f.i32_const(gex_abi::imports::BRK_BAD_SP);
        self.f.call(self.cx.breakpoint);
        self.f.end();
    }

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
        note_traffic(r);
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
    ///
    /// This is the traffic `gexc build` reports as a share of the module --
    /// 19% for SQLite, 20% for Lua -- and carrying the hottest of those
    /// registers in the block's *parameters* instead was tried and reverted.
    /// A tail call can pass arguments, so four of them can ride across a
    /// boundary in machine registers rather than through memory, and on a
    /// microbenchmark whose loop body is one call that is 20% faster.
    ///
    /// It does not carry over. tcc compiling its largest source was 27%
    /// *slower*, and Lua and SQLite were indistinguishable from unchanged
    /// across six interleaved A/B rounds. The context is one 896-byte
    /// structure that never leaves L1, so these stores and loads are nearly
    /// free in cycles however much of the module they are; what the change
    /// really did was hold four more values live across every function, and
    /// on real blocks -- which are bigger than a microbenchmark's -- that
    /// costs more in spills than the memory traffic ever cost. It also made
    /// modules 6% larger, because a register carried in a parameter still
    /// has to be spilled to the context before every host call and every
    /// safepoint.
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
            note_traffic(r);
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
                    VUnOp::ExtAddPairwiseU8 => {
                        self.f.simd(simd::I16X8_EXTADD_PAIRWISE_I8X16_U)
                    }
                    VUnOp::ExtAddPairwiseU16 => {
                        self.f.simd(simd::I32X4_EXTADD_PAIRWISE_I16X8_U)
                    }
                    VUnOp::ExtAddPairwiseS8 => {
                        self.f.simd(simd::I16X8_EXTADD_PAIRWISE_I8X16_S)
                    }
                    VUnOp::ExtAddPairwiseS16 => {
                        self.f.simd(simd::I32X4_EXTADD_PAIRWISE_I16X8_S)
                    }
                }
            }
            Expr::VExtract {
                a,
                lane,
                lane_bytes,
                signed,
                ty,
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
                // Every lane extract narrower than 64 bits produces an i32,
                // whatever the node says its type is. `smov x0, v0.h[2]` wants
                // the halfword sign-extended into a 64-bit register, and asked
                // for `I64`; without this the module fails validation, which
                // is how it was found -- in an exhaustive translation of
                // CPython, 182 MB into the wasm.
                if *ty == Ty::I64 && *lane_bytes < 8 {
                    self.f.byte(if *signed {
                        code::I64_EXTEND_I32_S
                    } else {
                        code::I64_EXTEND_I32_U
                    });
                }
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
    /// A fused multiply-add, without leaving the module.
    ///
    /// wasm has no FMA, so this would otherwise be a call to the shim's --
    /// and nbench's LU decomposition makes 706 million of them in one kernel.
    /// Removing the call is worth 6x on that kernel in the browser, 3.2x on
    /// NEURAL NET and 1.2x on FOURIER.
    ///
    /// The arithmetic and its guard are in [`crate::fma`], with the fuzzing
    /// that establishes them; this emits the same steps in wasm. Inlining at
    /// the site rather than calling a shared function is affordable because
    /// the sites are few even when the executions are not -- nbench's whole
    /// binary has about thirty-five.
    ///
    /// Returns false when the caller should emit the call after all.
    fn emit_fma(&mut self, args: &[Value]) -> bool {
        if args.len() != 4 {
            return false;
        }
        // The precision is a constant the lifter just made, so the shape is
        // decided here rather than at run time.
        let double = match self.value_const.get(args[3].0 as usize).copied().flatten() {
            Some(v) => v != 0,
            None => return false,
        };
        if double {
            self.emit_fma64(args);
        } else {
            self.emit_fmaf(args);
        }
        true
    }

    /// Push an f64 constant.
    fn f64_const(&mut self, v: f64) {
        self.f.byte(0x44);
        for b in v.to_bits().to_le_bytes() {
            self.f.byte(b);
        }
    }

    /// Take the f64 on the stack into a fresh local and leave it there.
    fn tee_f64(&mut self, l: u32) {
        self.f.local_tee(l);
    }

    /// Single precision: the product of two f32 is exact in f64, so only the
    /// sum can round, and round-to-odd stops the narrowing from breaking a
    /// tie the wrong way. No guard is needed.
    fn emit_fmaf(&mut self, args: &[Value]) {
        let (pl, sl, tl) = (
            self.f.add_local(ValType::F64),
            self.f.add_local(ValType::F64),
            self.f.add_local(ValType::F64),
        );
        // Each operand arrives as f32 bits in the low half of an i64.
        let mut widen = |me: &mut Self, v: Value| {
            me.push(v);
            me.f.byte(0xa7); // i32.wrap_i64
            me.f.byte(0xbe); // f32.reinterpret_i32
            me.f.byte(0xbb); // f64.promote_f32
        };
        widen(self, args[0]);
        widen(self, args[1]);
        self.f.byte(0xa2); // f64.mul -- exact
        self.tee_f64(pl);
        widen(self, args[2]);
        self.f.byte(0xa0); // f64.add
        self.tee_f64(sl);

        // t = (p - (s - bb)) + (c - bb), where bb = s - p.
        self.f.local_get(pl);
        self.f.local_get(sl);
        self.f.local_get(pl);
        self.f.byte(0xa1); // bb = s - p
        let bb = self.f.add_local(ValType::F64);
        self.f.local_tee(bb);
        // stack: p, s, bb -> p - (s - bb)
        self.f.byte(0xa1); // s - bb
        self.f.byte(0xa1); // p - (s - bb)
        widen(self, args[2]);
        self.f.local_get(bb);
        self.f.byte(0xa1); // c - bb
        self.f.byte(0xa0);
        self.f.local_set(tl);

        // if t != 0 and s is even, step s one ulp toward t.
        self.f.local_get(tl);
        self.f64_const(0.0);
        self.f.byte(0x61); // f64.eq
        self.f.byte(0x45); // i32.eqz -> t != 0
        self.f.local_get(sl);
        self.f.byte(0xbd); // i64.reinterpret_f64
        self.f.i64_const(1);
        self.f.byte(0x83); // i64.and
        self.f.byte(0x50); // i64.eqz -> low bit clear
        self.f.byte(0x71); // i32.and
        self.f.if_result(ValType::F64);
        {
            // Toward the error's sign, not simply upward: raising the
            // magnitude is backwards when the error is negative.
            self.f.local_get(sl);
            self.f.byte(0xbd);
            self.f.local_get(tl);
            self.f64_const(0.0);
            self.f.byte(0x64); // f64.gt -- t > 0
            self.f.local_get(sl);
            self.f64_const(0.0);
            self.f.byte(0x64); // s > 0
            self.f.byte(0x46); // i32.eq -- same sign
            self.f.if_result(ValType::I64);
            self.f.i64_const(1);
            self.f.else_();
            self.f.i64_const(-1);
            self.f.end();
            self.f.byte(0x7c); // i64.add
            self.f.byte(0xbf); // f64.reinterpret_i64
        }
        self.f.else_();
        self.f.local_get(sl);
        self.f.end();

        self.f.byte(0xb6); // f32.demote_f64
        self.f.byte(0xbc); // i32.reinterpret_f32
        self.f.byte(0xad); // i64.extend_i32_u -- the import's result shape
    }

    /// Double precision: Dekker's method behind an exponent guard, with the
    /// call it replaces in the else arm.
    fn emit_fma64(&mut self, args: &[Value]) {
        let l = |me: &mut Self| me.f.add_local(ValType::F64);
        let (al, bl, cl) = (l(self), l(self), l(self));
        self.push(args[0]);
        self.f.byte(0xbf);
        self.f.local_set(al);
        self.push(args[1]);
        self.f.byte(0xbf);
        self.f.local_set(bl);
        self.push(args[2]);
        self.f.byte(0xbf);
        self.f.local_set(cl);

        // The guard: every operand either zero, or with a biased exponent in
        // the band the fuzzing showed to be exact. The band cannot simply be
        // "finite" -- a*b doubles the exponent, so operands that are
        // individually fine overflow their product.
        for (i, v) in [al, bl, cl].iter().enumerate() {
            self.emit_in_fast_range(*v);
            if i > 0 {
                self.f.byte(0x71); // i32.and
            }
        }
        self.f.if_result(ValType::I64);
        self.emit_dekker(al, bl, cl);
        self.f.byte(0xbd); // i64.reinterpret_f64
        self.f.else_();
        // Everything the guard rejects goes where it used to.
        self.f.local_get(self.ctx_local);
        for a in args {
            self.push(*a);
        }
        self.f.call(self.cx.fma);
        self.f.end();
    }

    /// Leave 1 on the stack when the f64 in `v` is one the fast path handles.
    fn emit_in_fast_range(&mut self, v: u32) {
        let e = self.f.add_local(ValType::I64);
        self.f.local_get(v);
        self.f.byte(0xbd); // i64.reinterpret_f64
        self.f.i64_const(52);
        self.f.byte(0x87); // i64.shr_u
        self.f.i64_const(0x7ff);
        self.f.byte(0x83); // i64.and
        self.f.local_tee(e);
        self.f.i64_const(crate::fma::SAFE_LO as i64);
        self.f.byte(0x56); // i64.ge_u
        self.f.local_get(e);
        self.f.i64_const(crate::fma::SAFE_HI as i64);
        self.f.byte(0x58); // i64.le_u
        self.f.byte(0x71); // i32.and
        // Zero is admitted too, and is carried through exactly.
        self.f.local_get(v);
        self.f64_const(0.0);
        self.f.byte(0x61); // f64.eq
        self.f.byte(0x72); // i32.or
    }

    /// `p + e == a*b` exactly, `s + t == p + c` exactly, then one rounding.
    fn emit_dekker(&mut self, al: u32, bl: u32, cl: u32) {
        let lf = |me: &mut Self| me.f.add_local(ValType::F64);
        let (p, e, s, t, tmp) = (lf(self), lf(self), lf(self), lf(self), lf(self));
        let (ah, al2, bh, bl2) = (lf(self), lf(self), lf(self), lf(self));

        // p = a * b
        self.f.local_get(al);
        self.f.local_get(bl);
        self.f.byte(0xa2);
        self.f.local_set(p);

        // Split a and b into halves that multiply without error.
        for (x, hi, lo) in [(al, ah, al2), (bl, bh, bl2)] {
            self.f.local_get(x);
            self.f64_const(crate::fma::SPLIT);
            self.f.byte(0xa2);
            self.f.local_tee(tmp);
            self.f.local_get(tmp);
            self.f.local_get(x);
            self.f.byte(0xa1); // tmp - x
            self.f.byte(0xa1); // hi = tmp - (tmp - x)
            self.f.local_set(hi);
            self.f.local_get(x);
            self.f.local_get(hi);
            self.f.byte(0xa1); // lo = x - hi
            self.f.local_set(lo);
        }

        // e = ((ah*bh - p) + ah*bl + al*bh) + al*bl
        self.f.local_get(ah);
        self.f.local_get(bh);
        self.f.byte(0xa2);
        self.f.local_get(p);
        self.f.byte(0xa1);
        self.f.local_get(ah);
        self.f.local_get(bl2);
        self.f.byte(0xa2);
        self.f.byte(0xa0);
        self.f.local_get(al2);
        self.f.local_get(bh);
        self.f.byte(0xa2);
        self.f.byte(0xa0);
        self.f.local_get(al2);
        self.f.local_get(bl2);
        self.f.byte(0xa2);
        self.f.byte(0xa0);
        self.f.local_set(e);

        // s = p + c, and t its exact error.
        self.f.local_get(p);
        self.f.local_get(cl);
        self.f.byte(0xa0);
        self.f.local_set(s);
        self.f.local_get(s);
        self.f.local_get(p);
        self.f.byte(0xa1);
        self.f.local_set(tmp); // bb
        self.f.local_get(p);
        self.f.local_get(s);
        self.f.local_get(tmp);
        self.f.byte(0xa1);
        self.f.byte(0xa1);
        self.f.local_get(cl);
        self.f.local_get(tmp);
        self.f.byte(0xa1);
        self.f.byte(0xa0);
        self.f.local_set(t);

        // The one rounding.
        self.f.local_get(s);
        self.f.local_get(t);
        self.f.local_get(e);
        self.f.byte(0xa0);
        self.f.byte(0xa0);
    }

    fn host_call_inline(&mut self, func: HostFunc, args: &[Value]) {
        if func == HostFunc::Fma && self.emit_fma(args) {
            return;
        }
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
                // Poll here too, and not only on a backward direct branch.
                //
                // A backward direct branch is the cheap approximation of a
                // loop, and it misses the loop that matters most: one whose
                // back edge is computed. A bytecode interpreter's dispatch
                // is `br <reg>` through a table, and so is anything
                // jump-threaded -- CPython's, Mesa's. A thread inside one
                // reached no poll at all, which means it could not be
                // preempted, could not take a signal, could not be
                // interrupted by a timer, and did not appear in any trace.
                //
                // Measured: two hundred million iterations of a loop closed
                // only by `br x16` produced zero safepoints. The same loop
                // with a target the compiler could fold to a direct branch
                // produced 195. That difference is the bug, and it is why
                // glxgears went quiet at a full core with nothing to say.
                //
                // The context PC is already the target, stored above, and
                // the registers are already flushed -- so the slow path may
                // deliver a signal and unwind from here safely. The pc
                // passed is this branch's own, which is what a trace wants.
                self.emit_safepoint_no_flush(block.end - 4);
                self.emit_dispatch_tail_call();
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
    /// The safepoint countdown, for a caller that has already flushed and
    /// that carries on rather than jumping.
    ///
    /// `emit_safepoint` is written for a direct branch: it may flush inside
    /// a fused loop, and its caller follows it with a jump to a constant.
    /// An indirect branch has flushed already and its target is in a local,
    /// so it needs the counter and the call and nothing else.
    fn emit_safepoint_no_flush(&mut self, pc: u64) {
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
        self.f.local_get(self.ctx_local);
        self.f.i64_const(pc as i64);
        self.f.call(self.cx.safepoint);
        self.f.end();
    }

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
        // Written out above but deliberately not read back. The host only
        // changes a register on a path that does not come back here -- a
        // signal unwinds, a thread switch on the browser host traps out --
        // and every one of those resumes through the context. Reading them
        // back was tried and is what made a loop with a `continue` and a
        // `break` in it never finish.
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
        self.f.i64_const(target as i64);
        self.f.local_set(self.scratch_i64);
        self.emit_dispatch_tail_call();
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
        self.emit_dispatch_entry();
        self.mask_slot_bits();
        self.add_page_offset();
    }

    /// Load the directory entry for the address in `scratch_i64`.
    ///
    /// Split out from the index arithmetic because with several tables the
    /// entry is wanted twice -- once for its table index and once for its
    /// base slot -- and loading it twice would mean two loads on every
    /// indirect branch.
    fn emit_dispatch_entry(&mut self) {
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
    }

    /// Drop the table index from an entry on the stack, leaving the base slot.
    ///
    /// Skipped entirely with one table, where the top bits are always zero
    /// and the mask would be an instruction that can never change anything.
    fn mask_slot_bits(&mut self) {
        if gex_abi::DISPATCH_TABLES > 1 {
            self.f.i32_const(gex_abi::DISPATCH_SLOT_MASK as i32);
            self.f.byte(0x71); // i32.and
        }
    }

    /// index += (pc & 0xfff) >> 2
    fn add_page_offset(&mut self) {
        self.f.local_get(self.scratch_i64);
        self.f.i64_const((DISPATCH_PAGE - 1) as i64);
        self.f.byte(0x83); // i64.and
        self.f.i64_const(2);
        self.f.byte(0x88); // i64.shr_u
        self.f.byte(0xa7); // i32.wrap_i64
        self.f.byte(0x6a); // i32.add
    }

    /// Tail-call the block at the address in `scratch_i64`, through whichever
    /// dispatch table its page lives in.
    ///
    /// With one table this is what it always was: compute the index, then a
    /// single `return_call_indirect`. One table cannot hold a real
    /// application, though -- see `DISPATCH_TABLES` -- so the entry also
    /// carries which of several it means, in the eight bits above the slot,
    /// and the choice is a `br_table` over one tail call per table:
    ///
    ///     block            ; falls through to table 0, and is the default
    ///       block          ; ... to table 1
    ///         ...
    ///           local.get tbl
    ///           br_table N-1 ... 1 0 N-1
    ///         end
    ///         ; table N-1: ctx, index, return_call_indirect ty N-1
    ///       end
    ///       ...
    ///
    /// Entry zero still means "no translation": table 0, and a slot inside
    /// the reserved miss span at the bottom of it. So an untranslated page,
    /// an address past the directory's reach and a genuine table-0 block all
    /// take the same path with no test between them, which is the property
    /// the single-table version had and the one worth keeping.
    ///
    /// The index and the table are computed into locals before the blocks
    /// start, because a value pushed outside a `block` cannot be consumed
    /// inside one.
    fn emit_dispatch_tail_call(&mut self) {
        if gex_abi::DISPATCH_TABLES <= 1 {
            self.f.local_get(self.ctx_local);
            self.emit_dispatch_index();
            self.f.return_call_indirect(self.cx.block_ty, 0);
            return;
        }
        let n = gex_abi::DISPATCH_TABLES as u32;
        let entry = self.scratch_entry;
        let index = self.scratch_index;

        self.emit_dispatch_entry();
        self.f.local_tee(entry);
        self.mask_slot_bits();
        self.add_page_offset();
        self.f.local_set(index);

        // tbl = entry >> 24, reusing the entry local now its base is spent.
        self.f.local_get(entry);
        self.f.i32_const(gex_abi::DISPATCH_SLOT_BITS as i32);
        self.f.byte(0x76); // i32.shr_u
        self.f.local_set(entry);

        // `n` blocks, not `n-1`: the region *inside* the innermost one is
        // unreachable, because `br_table` always branches. Each `end` opens a
        // reachable region and there must be one per table.
        for _ in 0..n {
            self.f.block_void();
        }
        self.f.local_get(entry);
        // Depth 0 is the innermost block, so its `end` is the first region
        // emitted and belongs to the last table. Table k breaks to depth
        // `n-1-k`; the default is table 0's depth, which is where an entry
        // naming a table that does not exist ends up -- the miss stub, for a
        // directory that has been scribbled on.
        let targets: Vec<u32> = (0..n).map(|k| n - 1 - k).collect();
        self.f.br_table(&targets, n - 1);
        for k in (0..n).rev() {
            self.f.end();
            self.f.local_get(self.ctx_local);
            self.f.local_get(index);
            self.f.return_call_indirect(self.cx.block_ty, k);
        }
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

thread_local! {
    /// Regions formed, and how many blocks went into them.
    pub static REGIONS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
    /// Bytes emitted moving guest registers between the context and locals.
    /// That traffic is the price of a block boundary, and `gexc build`
    /// reports it as a share of the module.
    pub static CTX_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How often each guest register crosses a boundary, under
    /// `GEX_REG_HISTOGRAM`. Which registers those are is the first question
    /// anyone asks about that percentage.
    pub static REG_TRAFFIC: std::cell::RefCell<HashMap<GuestReg, usize>> =
        std::cell::RefCell::new(HashMap::new());
}

fn note_traffic(r: GuestReg) {
    REG_TRAFFIC.with(|t| *t.borrow_mut().entry(r).or_default() += 1);
}

/// Every guest register a block reads, and every one it writes.
///
/// Unlike [`live_in_registers`] this does not care about order within the
/// block: a region holds its registers in locals from entry to exit, so what
/// matters is only whether a register is read at all -- in which case the
/// local has to start out holding the context's value -- and whether it is
/// written at all -- in which case the context has to be caught up when
/// control leaves. Keeping the two apart is worth doing: a register the loop
/// only reads needs no store on the way out, and one it only writes needs no
/// load on the way in, and a region pays those on every entry and exit.
pub(crate) fn registers_read_and_written(block: &Block) -> (Vec<GuestReg>, Vec<GuestReg>) {
    let (mut read, mut written) = (Vec::new(), Vec::new());
    for s in &block.stmts {
        match s {
            Stmt::Def {
                expr: Expr::ReadReg(r),
                ..
            } => {
                if !read.contains(r) {
                    read.push(*r);
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
    (read, written)
}

/// Whether a statement can leave the register cache stale.

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

pub fn clobbers_guest_state(s: &Stmt) -> bool {
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
