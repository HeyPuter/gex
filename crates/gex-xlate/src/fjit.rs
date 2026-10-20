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
//! the next request, per guest process: the native runner keeps one
//! translator for every process in the tree (a shell, Xwayland and the game
//! it starts), whose images sit at the same addresses with different code, so
//! what one process's code taught must never be applied to another's.

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
    /// Acquisitions of `st`, and how many found it held (`GEX_FJIT_STATS`):
    /// translations from several threads (the browser's Workers, the
    /// native runner's) share one translator.
    locks: std::sync::atomic::AtomicU64,
    contended: std::sync::atomic::AtomicU64,
}

/// What recovery learned of one process's code ([`Jit::image`] starts it over).
#[derive(Default)]
struct Learned {
    /// Entries tier-1 serves with a host builtin (`memcpy` and friends, by
    /// symbol): their functions are the builtin, as in tier-1.
    intrinsics: std::sync::Arc<HashMap<u64, Intrinsic>>,
    /// Every function start seen: a call or jump to one is a call or tail
    /// call, and bounds the functions around it.
    starts: BTreeMap<u64, StartInfo>,
    noreturn: BTreeSet<u64>,
    /// Entries some translation holds.
    translated: HashSet<u64>,
    /// Entries a translation in progress (on another thread) holds: left
    /// out of other batches as the translated are, so two threads do not
    /// emit the same callees.
    in_flight: HashSet<u64>,
    /// Read-only segments of the images the loader told us about: a jump
    /// table read from one is as fixed as it is ahead of time. (Not
    /// tracked past `mprotect`: data the guest makes writable later counts
    /// as read-only still.)
    readonly: Vec<(u64, u64)>,
}

#[derive(Default)]
struct State {
    /// By process ([`Jit::translate_in`]'s `process`; 0 for a runner with one
    /// process per translator, as the browser shim is).
    procs: HashMap<u64, Learned>,
    /// Processes [`Jit::forget`] dropped: a translation still in flight for
    /// one must not bring its state back.
    gone: HashSet<u64>,
    /// Bumped by whatever invalidates what was learned ([`Jit::image`],
    /// [`Jit::retire`], [`Jit::forget`]): a translation that saw another
    /// value at its start was recovered from what is no longer so, and
    /// marks nothing translated.
    epoch: u64,
    /// For `GEX_FJIT_STATS`: translations, functions, time in recovery and
    /// emission, module bytes.
    modules: u64,
    functions: u64,
    recover_us: u64,
    emit_us: u64,
    bytes: u64,
}

impl Jit {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        use std::sync::atomic::Ordering::Relaxed;
        self.locks.fetch_add(1, Relaxed);
        match self.st.try_lock() {
            Ok(g) => g,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                self.contended.fetch_add(1, Relaxed);
                self.st.lock().unwrap_or_else(|e| e.into_inner())
            }
        }
    }

    pub fn new(intrinsics: HashMap<u64, Intrinsic>) -> Jit {
        let jit = Jit::default();
        jit.lock().procs.entry(0).or_default().intrinsics = std::sync::Arc::new(intrinsics);
        jit
    }

    /// Process `process` runs a new program (`image`, the file `elf`): it
    /// starts with what the loader knows of that image and nothing else --
    /// a new process, or an `execve` over an old one.
    pub fn image(&self, process: u64, intrinsics: HashMap<u64, Intrinsic>, image: &gex_ld::Image, elf: Option<&[u8]>) {
        {
            let mut st = self.lock();
            st.epoch += 1;
            st.procs.insert(process, Learned { intrinsics: std::sync::Arc::new(intrinsics), ..Learned::default() });
        }
        self.add_image_in(process, image, elf);
    }

    /// Process `process` unmapped or reprotected the code in `ranges`:
    /// what was learned there (function starts, never-returning functions,
    /// functions translated, read-only ranges) described the code that was
    /// there, and a `dlclose` and a later `dlopen` put another library at
    /// the same addresses. Kept, the old library's starts split the new
    /// one's functions where it had none: HK's SDL loads libdbus, unloads
    /// it, and maps libxcb over it; a libdbus start ended libxcb's
    /// `_xcb_in_read_block` before its `cmovle` as a tail call, flags dead,
    /// and it read the X setup reply out of an empty queue.
    pub fn retire(&self, process: u64, ranges: &[(u64, u64)]) {
        let mut st = self.lock();
        st.epoch += 1;
        let Some(l) = st.procs.get_mut(&process) else { return };
        for &(lo, hi) in ranges.iter().filter(|r| r.1 > r.0) {
            let gone: Vec<u64> = l.starts.range(lo..hi).map(|(&a, _)| a).collect();
            for a in gone {
                l.starts.remove(&a);
            }
            l.noreturn.retain(|&a| a < lo || a >= hi);
            l.translated.retain(|&a| a < lo || a >= hi);
            l.readonly.retain(|&(s, e)| e <= lo || s >= hi);
        }
    }

    /// Process `process` is gone.
    pub fn forget(&self, process: u64) {
        let mut st = self.lock();
        st.epoch += 1;
        st.procs.remove(&process);
        st.gone.insert(process);
    }

    /// The process replaced its program (`execve`): forget everything
    /// learned of the old one -- its function starts, never-returning
    /// functions, read-only ranges and builtins sit at addresses the new
    /// image now holds (a PIE replacing a PIE lands at the same base) --
    /// and start over from `image`.
    pub fn exec(&self, intrinsics: HashMap<u64, Intrinsic>, image: &gex_ld::Image, elf: Option<&[u8]>) {
        self.image(0, intrinsics, image, elf);
    }

    /// What the loader knows of an image the program maps: its function
    /// starts (symbols, `.eh_frame`, init arrays, relocations; `elf` is the
    /// file, for `.eh_frame` and ELF32 symbols), with their extents, which
    /// bound functions and computed-goto tables as they do ahead of time.
    /// Returns how many function starts it knows of the image.
    pub fn add_image(&self, image: &gex_ld::Image, elf: Option<&[u8]>) -> usize {
        self.add_image_in(0, image, elf)
    }

    fn add_image_in(&self, process: u64, image: &gex_ld::Image, elf: Option<&[u8]>) -> usize {
        let code = Code::with_elf(image, elf);
        let (starts, _) = func::seeds(&code, elf, false);
        let mut guard = self.lock();
        let st = guard.procs.entry(process).or_default();
        let mut n = 0;
        for (a, info) in starts {
            if info.strong() {
                st.starts.insert(a, info);
                n += 1;
            }
        }
        st.readonly.extend(
            image
                .segments
                .iter()
                .filter(|s| !s.writable)
                .map(|s| (s.vaddr, s.vaddr + s.bytes.len() as u64)),
        );
        n
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
        self.translate_in(0, mem, addr, executable, read_only, volatile, page_gen, class32)
    }

    /// [`Jit::translate`] for guest process `process`, whose learned state
    /// ([`Jit::image`]) is its own.
    #[allow(clippy::too_many_arguments)]
    pub fn translate_in(
        &self,
        process: u64,
        mem: &[u8],
        addr: u64,
        executable: &dyn Fn(u64) -> bool,
        read_only: &dyn Fn(u64, u64) -> bool,
        volatile: &dyn Fn(u64) -> bool,
        page_gen: &dyn Fn(u64) -> u32,
        class32: bool,
    ) -> Option<Vec<u8>> {
        self.translate_skipping(process, mem, addr, executable, &|_| false, read_only, volatile, page_gen, class32)
    }

    /// [`Jit::translate_in`], leaving out of the batch the callees `skip`
    /// names (served by another tier: the call reaches them through the
    /// table). `skip` bounds the batch only, never instruction fetch: a
    /// function another tier serves can have an entry in the middle of the
    /// one being recovered, and fetch taking its bytes for non-code decodes
    /// them as illegal.
    #[allow(clippy::too_many_arguments)]
    pub fn translate_skipping(
        &self,
        process: u64,
        mem: &[u8],
        addr: u64,
        executable: &dyn Fn(u64) -> bool,
        skip: &dyn Fn(u64) -> bool,
        read_only: &dyn Fn(u64, u64) -> bool,
        volatile: &dyn Fn(u64) -> bool,
        page_gen: &dyn Fn(u64) -> u32,
        class32: bool,
    ) -> Option<Vec<u8>> {
        if class32 && !gex_abi::X86 {
            return None;
        }
        if class32 != crate::arch::compat() {
            crate::set_compat(class32);
        }
        let started = now_us();
        if std::env::var("GEX_FJIT_STATS").as_deref() == Ok("2") {
            eprintln!("[fjit] translate {addr:#x}: bytes {:02x?}", &mem[addr as usize..addr as usize + 16]);
        }
        // Under the lock: recovery (it reads and extends what was learned).
        // Not under it: summaries and emission, most of the time -- other
        // threads' translations go on meanwhile (in the browser, a Worker
        // waiting here waits in `memory.atomic.wait`).
        let mut guard = self.lock();
        let epoch = guard.epoch;
        let State { procs, gone, .. } = &mut *guard;
        // A process already gone (a late request): what it learns goes nowhere.
        let mut scratch = Learned::default();
        let st = if gone.contains(&process) { &mut scratch } else { procs.entry(process).or_default() };
        let intrinsics = std::sync::Arc::clone(&st.intrinsics);
        let readonly = st.readonly.clone();
        let ro = |a: u64, n: u64| readonly.iter().any(|&(lo, hi)| a >= lo && a.saturating_add(n) <= hi) || read_only(a, n);
        let code = Code::memory(mem, executable, &ro);
        let opts = RecoverOptions::default();
        let mut batch: BTreeMap<u64, Function> = BTreeMap::new();
        let mut order: Vec<u64> = Vec::new();
        let mut work: VecDeque<u64> = VecDeque::from([addr]);
        let limit = batch_limit();
        while let Some(a) = work.pop_front() {
            if batch.len() >= limit {
                break;
            }
            if batch.contains_key(&a) || (a != addr && (st.translated.contains(&a) || st.in_flight.contains(&a) || skip(a))) || !executable(a) {
                continue;
            }
            let Learned { starts, noreturn, .. } = &mut *st;
            // Recovery records `a` as a function start for good, which bounds
            // every function recovered after it: a later recovery of the
            // function around `a` ends there as at a tail call, with the
            // flags dead. A root the table missed at can be the middle of a
            // function (a return or a signal's resume through dispatch, a
            // computed goto's label, a guessed slot target), so `a` stays a
            // start only if something already made it one (a call, the
            // image's symbols). The page's JIT, with no images to seed it,
            // split libxcb's `_xcb_in_read_block` before a `cmovle` this way.
            let before = starts.get(&a).cloned();
            let f = func::recover_one(&code, a, starts, noreturn, &opts);
            if !before.as_ref().is_some_and(|b| b.strong()) {
                match before {
                    Some(b) => {
                        starts.insert(a, b);
                    }
                    None => {
                        starts.remove(&a);
                    }
                }
            }
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
                if std::env::var_os("GEX_FJIT_STATS").is_some() {
                    eprintln!("[fjit] {a:#x}: code in writable memory; left to the fallback");
                }
                if a == addr {
                    return None;
                }
                continue;
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
        st.in_flight.extend(order.iter().copied());
        drop(guard);
        let sums = summary::analyze_functions(&batch).map;
        let recovered = now_us().saturating_sub(started);
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
            let _ = std::fs::write(format!("{dir}/p{process}-{addr:x}.gir"), text);
        }
        // A bug in the emitter costs the batch its translation, not the
        // process: the fallback runs it.
        let emitted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            gex_fjit::runtime(functions, &sums, class32, page_gen, &builtins, &pops)
        }))
        .map_err(|p| p.downcast_ref::<String>().cloned().or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string())).unwrap_or_default());
        let mut done = Vec::new();
        let out = match emitted {
            Err(why) => {
                eprintln!("[fjit] {addr:#x}: the emitter panicked ({why}); left to the fallback");
                None
            }
            Ok(Ok((wasm, entries, es))) if entries.contains(&addr) => {
                if std::env::var("GEX_FJIT_STATS").as_deref() == Ok("2") {
                    eprintln!("[fjit] {addr:#x}: {es:?}");
                }
                done = entries;
                if let Ok(dir) = std::env::var("GEX_FJIT_KEEP") {
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::fs::write(format!("{dir}/p{process}-{addr:x}.wasm"), &wasm);
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
        let mut guard = self.lock();
        let same = guard.epoch == epoch;
        let State { procs, modules, functions: n_functions, recover_us, emit_us, bytes, .. } = &mut *guard;
        if let Some(st) = procs.get_mut(&process) {
            for a in &order {
                st.in_flight.remove(a);
            }
            if same {
                st.translated.extend(done);
            }
        }
        *modules += 1;
        *n_functions += n as u64;
        *recover_us += recovered;
        *emit_us += now_us().saturating_sub(started).saturating_sub(recovered);
        *bytes += out.as_ref().map_or(0, |w| w.len() as u64);
        if std::env::var_os("GEX_FJIT_STATS").is_some() && modules.is_power_of_two() {
            eprintln!("[fjit] {}; {}", stats_line(*modules, *n_functions, *recover_us, *emit_us, *bytes), self.lock_line());
        }
        out
    }

    /// What the JIT has done, for `GEX_FJIT_STATS` and `GEX_NT_STATS`.
    pub fn stats(&self) -> String {
        let line = self.lock().line();
        format!("{line}; {}", self.lock_line())
    }

    /// How often the translator's lock was found held.
    fn lock_line(&self) -> String {
        use std::sync::atomic::Ordering::Relaxed;
        format!("lock {} of {} contended", self.contended.load(Relaxed), self.locks.load(Relaxed))
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
        stats_line(self.modules, self.functions, self.recover_us, self.emit_us, self.bytes)
    }
}

fn stats_line(modules: u64, functions: u64, recover_us: u64, emit_us: u64, bytes: u64) -> String {
    let n = functions.max(1);
    format!(
        "{modules} modules, {functions} functions; recovery {:.1} ms ({} us/function), emission {:.1} ms ({} us/function), {bytes} wasm bytes ({}/function)",
        recover_us as f64 / 1e3,
        recover_us / n,
        emit_us as f64 / 1e3,
        emit_us / n,
        bytes / n
    )
}


/// Microseconds since the first call, for the translation statistics. 0 on
/// wasm32/wasm64-unknown-unknown, where `std::time::Instant` panics (the browser's
/// function JIT); its stats then report no time.
fn now_us() -> u64 {
    #[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
    {
        static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        T0.get_or_init(std::time::Instant::now).elapsed().as_micros() as u64
    }
    #[cfg(all(target_family = "wasm", target_os = "unknown"))]
    {
        0
    }
}
