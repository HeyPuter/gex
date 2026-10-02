//! The function JIT at run time (docs/merge-plan.md §9.8, stream F): what
//! the function table's host asks for on a lookup miss (`gex_kernel::ftab`'s
//! `Translator`, installed by `gexc run --fjit`).
//!
//! A request names one address. The function there is recovered from guest
//! memory as it is now (`func::recover_one`), and with it the direct callees
//! no earlier translation holds, breadth first, up to [`BATCH`] of them: one
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
    intrinsics: HashMap<u64, Intrinsic>,
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
        Jit { intrinsics, ..Jit::default() }
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
        let mut st = self.st.lock().unwrap_or_else(|e| e.into_inner());
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
            if batch.contains_key(&a) || (a != addr && st.translated.contains(&a)) || !executable(a) {
                continue;
            }
            let State { starts, noreturn, .. } = &mut *st;
            let f = func::recover_one(&code, a, starts, noreturn, &opts);
            if let Err(e) = f.verify() {
                if std::env::var_os("GEX_FJIT_STATS").is_some() {
                    eprintln!("[fjit] {a:#x}: recovered malformed ({e}); left to the fallback");
                }
                if a == addr {
                    return None;
                }
                continue;
            }
            if !self.intrinsics.contains_key(&a) {
                for c in f.call_sites() {
                    if let Callee::Direct(t) = c.callee {
                        work.push_back(t);
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
            .filter_map(|a| self.intrinsics.get(a).map(|&k| crate::intrinsic::block(*a, k)))
            .collect();
        let pops = ret_pops(&functions, class32, &|a| mem.get(a as usize).copied());
        let out = match gex_fjit::runtime(functions, &sums, class32, page_gen, &builtins, &pops) {
            Ok((wasm, entries, es)) if entries.contains(&addr) => {
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
            Ok(_) => {
                if std::env::var_os("GEX_FJIT_STATS").is_some() {
                    eprintln!("[fjit] {addr:#x}: the module does not hold it; left to the fallback");
                }
                None
            }
            Err(e) => {
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

/// Where a function's `ret` pops more than the return address (i386
/// `ret N`: stdcall callees, struct returns): the terminator's pc and how
/// far it moves SP, for the function JIT's return check, which otherwise
/// sees SP past where the caller expects and leaves. `read` gives a guest
/// byte. Empty off x86.
pub fn ret_pops(functions: &[Function], class32: bool, read: &dyn Fn(u64) -> Option<u8>) -> HashMap<u64, u64> {
    let mut out = HashMap::new();
    if !gex_abi::X86 {
        return out;
    }
    let ptr = if class32 { 4 } else { 8 };
    for f in functions {
        for b in &f.blocks {
            if !matches!(b.term, gex_ir::func::FTerm::Return { .. }) || b.synthetic {
                continue;
            }
            let mut a = b.term_pc;
            // `rep ret` / `bnd ret`; an operand-size prefix (a 16-bit
            // pop) gets no entry, and its check leaves.
            while matches!(read(a), Some(0xf2 | 0xf3)) && a < b.term_pc + 4 {
                a += 1;
            }
            if read(a) == Some(0xc2) {
                if let (Some(lo), Some(hi)) = (read(a + 1), read(a + 2)) {
                    let n = u16::from_le_bytes([lo, hi]) as u64;
                    if n != 0 {
                        out.insert(b.term_pc, ptr + n);
                    }
                }
            }
        }
    }
    out
}

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

