//! The function JIT at run time (docs/merge-plan.md §9.8, stream F): what
//! the function table's host asks for on a lookup miss (`gex_kernel::ftab`'s
//! `Translator`, installed by `gexc run --fjit`).
//!
//! A request names one address. The function there is recovered from guest
//! memory as it is now (`func::recover_one`), and with it the direct callees
//! no earlier translation holds (and what their `call [slot]`s and
//! `jmp [slot]`s point at now), breadth first, up to [`BATCH`] of them: one
//! module, the calls among them direct `call`s, summaries over the batch
//! (callees outside it count as unknown). What recovery learns -- function
//! starts, which functions never return, what is translated -- is kept for
//! the next request.

use crate::func::{self, Code, RecoverOptions, StartInfo};
use crate::intrinsic::Intrinsic;
use crate::summary;
use gex_ir::func::{Callee, Function};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Mutex;

/// Functions per translation: the one asked for and its untranslated
/// callees. `GEX_FJIT_BATCH` overrides it.
pub const BATCH: usize = 64;

fn batch_limit() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| std::env::var("GEX_FJIT_BATCH").ok().and_then(|v| v.parse().ok()).unwrap_or(BATCH).max(1))
}

/// The process's run-time function JIT.
#[derive(Default)]
pub struct Jit {
    st: Mutex<State>,
    /// Entries tier-1 serves with a host builtin (`memcpy` and friends, by
    /// symbol): their functions are the builtin, as in tier-1.
    /// Replaced on `execve` ([`Jit::exec`]).
    intrinsics: std::sync::RwLock<HashMap<u64, Intrinsic>>,
}

#[derive(Default)]
struct State {
    /// Every function start seen: a call or jump to one is a call or tail
    /// call, and bounds the functions around it.
    starts: BTreeMap<u64, StartInfo>,
    noreturn: BTreeSet<u64>,
    /// Entries some translation holds.
    translated: HashSet<u64>,
    /// Read-only segments of the images the loader told us about: a jump
    /// table read from one is as fixed as it is ahead of time. (Not
    /// tracked past `mprotect`: data the guest makes writable later counts
    /// as read-only still.)
    readonly: Vec<(u64, u64)>,
    /// For `GEX_FJIT_STATS`: translations, functions, time in recovery and
    /// emission, module bytes.
    modules: u64,
    functions: u64,
    recover_us: u64,
    emit_us: u64,
    bytes: u64,
}

impl Jit {
    pub fn new(intrinsics: HashMap<u64, Intrinsic>) -> Jit {
        Jit { intrinsics: std::sync::RwLock::new(intrinsics), ..Jit::default() }
    }

    /// The process replaced its program (`execve`): forget everything
    /// learned of the old one -- its function starts, never-returning
    /// functions, read-only ranges and builtins sit at addresses the new
    /// image now holds (a PIE replacing a PIE lands at the same base) --
    /// and start over from `image`.
    pub fn exec(&self, intrinsics: HashMap<u64, Intrinsic>, image: &gex_ld::Image, elf: Option<&[u8]>) {
        *self.intrinsics.write().unwrap_or_else(|e| e.into_inner()) = intrinsics;
        {
            let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
            let State { modules, functions, recover_us, emit_us, bytes, .. } = std::mem::take(&mut *st);
            *st = State { modules, functions, recover_us, emit_us, bytes, ..State::default() };
        }
        self.add_image(image, elf);
    }

    /// What the loader knows of an image the program maps: its function
    /// starts (symbols, `.eh_frame`, init arrays, relocations; `elf` is the
    /// file, for `.eh_frame` and ELF32 symbols), with their extents, which
    /// bound functions and computed-goto tables as they do ahead of time.
    pub fn add_image(&self, image: &gex_ld::Image, elf: Option<&[u8]>) {
        let code = Code::with_elf(image, elf);
        let (starts, _) = func::seeds(&code, elf, false);
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        for (a, info) in starts {
            if info.strong() {
                st.starts.insert(a, info);
            }
        }
        st.readonly.extend(
            image
                .segments
                .iter()
                .filter(|s| !s.writable)
                .map(|s| (s.vaddr, s.vaddr + s.bytes.len() as u64)),
        );
    }

    /// A module for the function at `addr` (see the module documentation),
    /// or `None` to leave it to the fallback. `page_gen` gives each page's
    /// `PAGE_GEN` value, which the functions check on entry.
    pub fn translate(
        &self,
        mem: &[u8],
        addr: u64,
        executable: &dyn Fn(u64) -> bool,
        read_only: &dyn Fn(u64, u64) -> bool,
        volatile: &dyn Fn(u64) -> bool,
        page_gen: &dyn Fn(u64) -> u32,
        class32: bool,
    ) -> Option<Vec<u8>> {
        if gex_abi::PPC || (class32 && !gex_abi::X86) {
            return None;
        }
        if class32 != crate::arch::compat() {
            crate::set_compat(class32);
        }
        let started = std::time::Instant::now();
        if std::env::var("GEX_FJIT_STATS").as_deref() == Ok("2") {
            eprintln!("[fjit] translate {addr:#x}: bytes {:02x?}", &mem[addr as usize..addr as usize + 16]);
        }
        let intrinsics = self.intrinsics.read().unwrap_or_else(|e| e.into_inner());
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
        let readonly = st.readonly.clone();
        let ro = |a: u64, n: u64| readonly.iter().any(|&(lo, hi)| a >= lo && a.saturating_add(n) <= hi) || read_only(a, n);
        let code = Code::memory(mem, executable, &ro);
        let opts = RecoverOptions::default();
        let mut batch: BTreeMap<u64, Function> = BTreeMap::new();
        let mut guards = gex_fjit::Guards::new();
        let mut order: Vec<u64> = Vec::new();
        let mut work: VecDeque<u64> = VecDeque::from([addr]);
        let limit = batch_limit();
        while let Some(a) = work.pop_front() {
            if batch.len() >= limit {
                break;
            }
            if batch.contains_key(&a) || (a != addr && st.translated.contains(&a)) || !executable(a) {
                continue;
            }
            let State { starts, noreturn, .. } = &mut *st;
            let f = func::recover_one(&code, a, starts, noreturn, &opts);
            if let Err(e) = f.verify().and_then(|_| block_local(&f)) {
                if std::env::var_os("GEX_FJIT_STATS").is_some() {
                    eprintln!("[fjit] {a:#x}: recovered malformed ({e}); left to the fallback");
                }
                if a == addr {
                    return None;
                }
                continue;
            }
            // Code in writable-and-executable memory (a JIT's) may be
            // rewritten with nothing to announce it: tier-1 checks each
            // block's bytes there (`Block::guard`); a function checks only
            // its pages' generations on entry, so it would run whatever a
            // direct jump reached when it was translated -- in a JIT's
            // fresh area, often bytes not yet written. It stays tier-1.
            if f.blocks.iter().any(|b| !b.synthetic && (volatile(b.start) || volatile(b.end.max(b.start + 1) - 1))) {
                if wx_refused() {
                    if std::env::var_os("GEX_FJIT_STATS").is_some() {
                        eprintln!("[fjit] {a:#x}: code in writable memory; left to the fallback");
                    }
                    if a == addr {
                        return None;
                    }
                    continue;
                }
                // Its bytes, compared on entry (`gex_fjit::Guards`).
                let mut spans: Vec<(u64, u64)> = f.blocks.iter().filter(|b| !b.synthetic).map(|b| (b.start, b.end.max(b.start + 1))).collect();
                spans.sort_unstable();
                let mut merged: Vec<(u64, u64)> = Vec::new();
                for (lo, hi) in spans {
                    match merged.last_mut() {
                        Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
                        _ => merged.push((lo, hi)),
                    }
                }
                let ranges: Vec<(u64, Vec<u8>)> = merged
                    .into_iter()
                    .filter_map(|(lo, hi)| mem.get(lo as usize..hi as usize).map(|b| (lo, b.to_vec())))
                    .collect();
                guards.insert(a, ranges);
            }
            if !intrinsics.contains_key(&a) {
                for c in f.call_sites() {
                    match c.callee {
                        Callee::Direct(t) => work.push_back(t),
                        // `call [iat]`: still a call through the table, but
                        // the target the slot holds now comes in this batch
                        // rather than a miss of its own (a guess: wrong only
                        // costs its translation).
                        Callee::Indirect(v) => {
                            if let Some(t) = slot_target(&f, c.block, v, mem) {
                                work.push_back(t);
                            }
                        }
                    }
                }
                // And `jmp [slot]` that recovery left unresolved, likewise.
                for id in f.ids() {
                    if let gex_ir::func::FTerm::Unresolved { target } = f.block(id).term {
                        if let Some(t) = slot_target(&f, id, target, mem) {
                            work.push_back(t);
                        }
                    }
                }
            }
            if std::env::var("GEX_FJIT_STATS").as_deref() == Ok("2") {
                eprintln!("[fjit]   {a:#x}: {} blocks", f.blocks.len());
                for b in &f.blocks {
                    if let gex_ir::func::FTerm::Switch { cases, table, .. } = &b.term {
                        eprintln!("[fjit]     switch at {:#x}: {} cases {:x?} {:?}", b.term_pc, cases.len(), cases.iter().map(|c| c.0).collect::<Vec<_>>(), table);
                    }
                    if let gex_ir::func::FTerm::Unresolved { .. } = &b.term {
                        eprintln!("[fjit]     unresolved at {:#x}", b.term_pc);
                    }
                }
            }
            order.push(a);
            batch.insert(a, f);
        }
        for (a, r) in func::may_return(&batch) {
            if !r {
                st.noreturn.insert(a);
            }
        }
        let sums = summary::analyze_functions(&batch).map;
        let recovered = started.elapsed();
        let functions: Vec<Function> = order.iter().filter_map(|a| batch.remove(a)).collect();
        let n = functions.len();
        let builtins: Vec<gex_ir::Block> = order
            .iter()
            .filter_map(|a| intrinsics.get(a).map(|&k| crate::intrinsic::block(*a, k)))
            .collect();
        let pops = ret_pops(&functions, class32, &|a| mem.get(a as usize).copied());
        if let Ok(dir) = std::env::var("GEX_FJIT_DUMP") {
            // The batch as GIR text (`gex_ir::text`), to replay offline.
            let _ = std::fs::create_dir_all(&dir);
            let text: String = functions.iter().map(gex_ir::text::print).collect::<Vec<_>>().join("\n");
            let _ = std::fs::write(format!("{dir}/{addr:x}.gir"), text);
        }
        // A bug in the emitter costs the batch its translation, not the
        // process: the fallback runs it.
        let emitted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            gex_fjit::runtime(functions, &sums, class32, page_gen, &builtins, &pops, &guards)
        }))
        .map_err(|p| p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default());
        let out = match emitted {
            Err(why) => {
                eprintln!("[fjit] {addr:#x}: the emitter panicked ({why}); left to the fallback");
                None
            }
            Ok(Ok((wasm, entries, es))) if entries.contains(&addr) => {
                if std::env::var("GEX_FJIT_STATS").as_deref() == Ok("2") {
                    eprintln!("[fjit] {addr:#x}: {es:?}");
                }
                st.translated.extend(entries);
                if let Ok(dir) = std::env::var("GEX_FJIT_KEEP") {
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::fs::write(format!("{dir}/{addr:x}.wasm"), &wasm);
                }
                Some(wasm)
            }
            Ok(Ok(_)) => {
                if std::env::var_os("GEX_FJIT_STATS").is_some() {
                    eprintln!("[fjit] {addr:#x}: the module does not hold it; left to the fallback");
                }
                None
            }
            Ok(Err(e)) => {
                eprintln!("[fjit] {addr:#x}: {e:#}; left to the fallback");
                None
            }
        };
        st.modules += 1;
        st.functions += n as u64;
        st.recover_us += recovered.as_micros() as u64;
        st.emit_us += (started.elapsed() - recovered).as_micros() as u64;
        st.bytes += out.as_ref().map_or(0, |w| w.len() as u64);
        if std::env::var_os("GEX_FJIT_STATS").is_some() && st.modules.is_power_of_two() {
            eprintln!("[fjit] {}", st.line());
        }
        out
    }

    /// What the JIT has done, for `GEX_FJIT_STATS` and `GEX_NT_STATS`.
    pub fn stats(&self) -> String {
        self.st.lock().unwrap_or_else(|e| e.into_inner()).line()
    }
}

/// Memory form's rule, which the emitter relies on: every value a block
/// uses is defined earlier in the same block. (Recovery has been seen to
/// break it on code that jumps into the middle of a lifted block, in a
/// packed section of noita.exe.)
fn block_local(f: &Function) -> Result<(), String> {
    for b in &f.blocks {
        let mut defined = HashSet::new();
        let mut bad = None;
        for s in &b.stmts {
            s.for_each_use(|v| {
                if !defined.contains(&v.0) {
                    bad = Some(v);
                }
            });
            gex_ir::func::for_each_def(s, |v| {
                defined.insert(v.0);
            });
        }
        b.term.for_each_use(|v| {
            if !defined.contains(&v.0) {
                bad = Some(v);
            }
        });
        if let Some(v) = bad {
            return Err(format!("block {:#x} uses {v:?} from another block", b.start));
        }
    }
    Ok(())
}

/// The pointer an indirect call's target `v` (in block `b`) loads from a
/// constant address (`call [iat]`, `jmp [iat]`), as memory holds it now.
fn slot_target(f: &Function, b: gex_ir::func::BlockId, v: gex_ir::Value, mem: &[u8]) -> Option<u64> {
    use gex_ir::{Expr, Stmt};
    let blk = f.block(b);
    let def = |x: gex_ir::Value| {
        blk.stmts.iter().find_map(|s| match s {
            Stmt::Def { dst, expr } if *dst == x => Some(expr),
            _ => None,
        })
    };
    // i386: the pointer zero-extended.
    let v = match def(v) {
        Some(Expr::Extend { a, signed: false }) => *a,
        _ => v,
    };
    let Some(Expr::Load { addr, offset, sz, .. }) = def(v) else { return None };
    let base = match def(*addr)? {
        Expr::ConstI64(c) => *c,
        Expr::ConstI32(c) => *c as u64,
        _ => return None,
    };
    let at = base.wrapping_add(*offset as u64);
    let n = sz.bytes() as u64;
    if !(n == 4 || n == 8) {
        return None;
    }
    let bytes = mem.get(at as usize..at.checked_add(n)? as usize)?;
    Some(if n == 4 { u32::from_le_bytes(bytes.try_into().ok()?) as u64 } else { u64::from_le_bytes(bytes.try_into().ok()?) })
}

/// Where a function's `ret` pops more than the return address
/// (`gex_cgen::func::ret_pops`, which the AOT tier's return check uses too).
pub use gex_cgen::func::ret_pops;

impl State {
    fn line(&self) -> String {
        let n = self.functions.max(1);
        format!(
            "{} modules, {} functions; recovery {:.1} ms ({} us/function), emission {:.1} ms ({} us/function), {} wasm bytes ({}/function)",
            self.modules,
            self.functions,
            self.recover_us as f64 / 1e3,
            self.recover_us / n,
            self.emit_us as f64 / 1e3,
            self.emit_us / n,
            self.bytes,
            self.bytes / n
        )
    }
}


/// `GEX_FJIT_WX=refuse`: leave code in writable and executable memory to
/// tier-1's block guards instead of translating it with a byte check on
/// entry (`gex_fjit::Guards`).
fn wx_refused() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("GEX_FJIT_WX").as_deref() == Ok("refuse"))
}
