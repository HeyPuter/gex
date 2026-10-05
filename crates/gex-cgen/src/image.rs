//! Whole-image function-tier builds with bounded memory (docs/function-tier.md
//! §11): what the NT layer installs for a PE image, and what `gex-eject`
//! needs for whole programs.
//!
//! Two stages, so the expensive one runs once:
//!
//! 1. [`write_recovery`]: the recovered functions of an image, in GIR's text
//!    form, one record each, plus a line per function of what code
//!    generation needs from the call-graph summaries and the trap-free
//!    analysis. Written to a temporary directory and renamed into place, so a
//!    killed run never leaves a cache a later build trusts. Every file starts
//!    with [`FORMAT`]; any other version is refused.
//! 2. [`build_c_module`]: read the functions back one at a time, emit C in
//!    units of about [`UNIT_BYTES`], compile each with clang (a precompiled
//!    `gex_abi.h`, at most `jobs` at once, and an object cache keyed by the
//!    unit's content, the header and the flags), and link everything into one
//!    module exporting `e_<entry>` for every function. That module does not
//!    depend on dispatch slots: it is spliced over tier-1 stubs at the
//!    entries (`splice::splice`) wherever and whenever the image is placed
//!    at the base it was built for.
//!
//! Peak memory in stage 2 is one unit of C plus the per-function metadata,
//! whatever the image's size.

use crate::func::{self, FnUnit};
use crate::{header, toolchain};
use anyhow::{anyhow, bail, Context, Result};
use gex_ir::func::{Function, RegSet, Summary};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

/// The first line of every file a recovery cache holds.
pub const FORMAT: &str = "gex-cgen-image 3";

/// A hash of the sources that decide an image's recovery (build.rs).
pub const RECOVERY_INPUTS: &str = env!("GEX_CGEN_RECOVERY_INPUTS");

/// A hash of the sources that decide a unit from a recovery (build.rs).
pub const UNIT_INPUTS: &str = env!("GEX_CGEN_UNIT_INPUTS");

/// What a unit embeds from outside gex-cgen and gex-abi (gex-emit's FMA
/// constants, in `gex_abi.h`), which the unit key does not hash: bump it
/// whenever [`unit_abi_fingerprint`] changes (its test says when).
pub const UNIT_ABI_VERSION: u32 = 1;

/// The identity of an image's recovery (`gex_key::recovery_key` with
/// [`RECOVERY_INPUTS`]): where the recovery is cached.
pub fn recovery_key(image: &gex_ld::Image) -> String {
    gex_key::recovery_key(image, RECOVERY_INPUTS)
}

/// The identity of an image's unit (`gex_key::unit_key` with this format and
/// these inputs): the directory under the cgen cache that holds it. Every
/// host finds units by this one function.
pub fn image_key(image: &gex_ld::Image) -> String {
    gex_key::unit_key(image, FORMAT, &unit_inputs(), UNIT_ABI_VERSION, RECOVERY_INPUTS)
}

/// [`UNIT_INPUTS`] and a hash of [`abi_fingerprint`]: everything that
/// decides a unit from a recovery.
fn unit_inputs() -> String {
    static S: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let base = S
        .get_or_init(|| {
            let mut h = gex_key::sha256::Sha256::new();
            h.update(abi_fingerprint().as_bytes());
            format!("{UNIT_INPUTS}:{}", gex_key::sha256::hex(&h.finish()))
        })
        .clone();
    // The flavor is part of the key: native and browser units never mix.
    let base = if unguarded() { format!("{base}:unguarded") } else { base };
    let base = if narrow_stores() { base } else { format!("{base}:widest") };
    // So is the guest memory (docs/mem64.md): a wasm64 unit is a different
    // module. Nothing is added for wasm32, whose keys stay as they were.
    if gex_abi::mem64() {
        format!("{base}:mem64")
    } else {
        base
    }
}

/// Store an i386 program's general-purpose registers to the low 32 bits of
/// their 64-bit context slots, as pewasm's CpuState fields are 32-bit
/// (3-4% of the wasm of Noita's large hot functions). Exact because every
/// other writer of those slots zero-extends (efcb8bfc: the host's syscall
/// results, signal frames, the NT layer's contexts; GEX_CHECK_I386_HIGH=1
/// checks it), so the high half is always zero. `GEX_CGEN_NARROW_STORES=0`
/// turns it off. In the unit key.
pub fn narrow_stores() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("GEX_CGEN_NARROW_STORES").as_deref() != Ok("0"))
}

static UNGUARDED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Build and look up the unguarded flavor of i386 units (no bounds guard on
/// guest accesses: `Emitter::unguarded`), for a host whose guest memory is
/// the whole linear memory, so out-of-range accesses trap by themselves --
/// natively. The browser's NT layer keeps host data above a lower guard
/// limit and keeps the guarded flavor (docs/noita-perf.md). Set before the
/// first key is computed.
pub fn set_unguarded(on: bool) {
    UNGUARDED.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn unguarded() -> bool {
    UNGUARDED.load(std::sync::atomic::Ordering::Relaxed)
}

/// What a unit's C takes from gex-abi: the generated header, and every value
/// gex-cgen formats into C directly (context offsets, dispatch constants).
/// The test `abi_fingerprint_covers_what_the_c_embeds` fails when the emitter
/// names a gex-abi value this does not list.
pub fn abi_fingerprint() -> String {
    use gex_abi::ctx;
    let mut s = crate::header::header();
    let named: &[(&str, u64)] = &[
        ("X86", gex_abi::X86 as u64),
        ("PPC", gex_abi::PPC as u64),
        ("DISPATCH_SHIFT", gex_abi::DISPATCH_SHIFT as u64),
        ("INST_ALIGN_SHIFT", gex_abi::INST_ALIGN_SHIFT as u64),
        ("INST_ALIGN", gex_abi::INST_ALIGN),
        ("DISPATCH_PAGE", gex_abi::DISPATCH_PAGE),
        ("SYSCALL_INST_BYTES", gex_abi::SYSCALL_INST_BYTES),
        ("ctx::SP", ctx::SP as u64),
        ("ctx::PC", ctx::PC as u64),
        ("ctx::NZCV", ctx::NZCV as u64),
        ("ctx::NF", ctx::NF as u64),
        ("ctx::ZF", ctx::ZF as u64),
        ("ctx::CF", ctx::CF as u64),
        ("ctx::VF", ctx::VF as u64),
        ("ctx::PF", ctx::PF as u64),
        ("ctx::AF", ctx::AF as u64),
        ("ctx::DF", ctx::DF as u64),
        ("ctx::TPIDR", ctx::TPIDR as u64),
        ("ctx::TPIDRRO", ctx::TPIDRRO as u64),
        ("ctx::FS_BASE", ctx::FS_BASE as u64),
        ("ctx::GS_BASE", ctx::GS_BASE as u64),
        ("ctx::MON_ADDR", ctx::MON_ADDR as u64),
        ("ctx::MON_VALUE", ctx::MON_VALUE as u64),
        ("ctx::MON_SIZE", ctx::MON_SIZE as u64),
        ("ctx::FPCR", ctx::FPCR as u64),
        ("ctx::FPSR", ctx::FPSR as u64),
        ("ctx::MXCSR", ctx::MXCSR as u64),
        ("ctx::SAFEPOINT_COUNTER", ctx::SAFEPOINT_COUNTER as u64),
        ("ctx::BALLAST", ctx::BALLAST as u64),
        ("ctx::LAST_TARGET", ctx::LAST_TARGET as u64),
        ("ctx::LAST_SOURCE", ctx::LAST_SOURCE as u64),
        ("ctx::X87", ctx::X87 as u64),
        ("ctx::X87_TOP", ctx::X87_TOP as u64),
        ("ctx::X87_CW", ctx::X87_CW as u64),
        ("ctx::X87_SW", ctx::X87_SW as u64),
        ("ctx::X87_TAGS", ctx::X87_TAGS as u64),
        ("ctx::PPC_CR", ctx::PPC_CR as u64),
        ("ctx::PPC_LR", ctx::PPC_LR as u64),
        ("ctx::PPC_CTR", ctx::PPC_CTR as u64),
        ("ctx::PPC_SO", ctx::PPC_SO as u64),
        ("ctx::PPC_OV", ctx::PPC_OV as u64),
        ("ctx::PPC_CA", ctx::PPC_CA as u64),
        ("ctx::PPC_XER_BC", ctx::PPC_XER_BC as u64),
        ("ctx::PPC_FPSCR", ctx::PPC_FPSCR as u64),
    ];
    for (n, v) in named {
        s.push_str(&format!("{n}={v}\n"));
    }
    for i in 0..32u8 {
        s.push_str(&format!("ctx::x({i})={} ctx::v({i})={} ctx::f({i})={}\n", ctx::x(i), ctx::v(i), ctx::f(i)));
    }
    s
}

/// The embedded constants [`UNIT_ABI_VERSION`] stands for.
pub fn unit_abi_fingerprint() -> String {
    format!("fma {:?} {} {}", gex_emit::fma::SPLIT, gex_emit::fma::SAFE_LO, gex_emit::fma::SAFE_HI)
}

/// C per translation unit before it is written and compiled.
pub const UNIT_BYTES: usize = 512 << 10;

/// What code generation reads of a function's summary (the rest stays
/// "unknown", which is always sound).
fn summary_line(entry: u64, s: &Summary, trapfree: Option<u32>, touched: RegSet, guardfree: bool) -> String {
    format!(
        "{entry:x} {:x} {:x} {}{}{} {} {:x} {}\n",
        s.inputs.0,
        s.preserved.0,
        s.may_unwind as u8,
        s.may_leave_pending as u8,
        s.unknown_callee as u8,
        trapfree.map(|w| w as i64).unwrap_or(-1),
        touched.0,
        guardfree as u8
    )
}

/// The line's sixth field, the registers the function's call tree names
/// (`func::touched`), is optional: a recovery written before it existed
/// reads as touching everything.
fn parse_summary_line(line: &str) -> Result<(u64, Summary, Option<u32>, RegSet, bool)> {
    let w: Vec<&str> = line.split_whitespace().collect();
    if !(5..=7).contains(&w.len()) || w[3].len() != 3 {
        bail!("malformed summary line {line:?}");
    }
    let hex = |s: &str| u128::from_str_radix(s, 16).map_err(|e| anyhow!("{line:?}: {e}"));
    let mut s = Summary::unknown();
    s.inputs = RegSet(hex(w[1])?);
    s.preserved = RegSet(hex(w[2])?);
    let f = w[3].as_bytes();
    s.may_unwind = f[0] == b'1';
    s.may_leave_pending = f[1] == b'1';
    s.unknown_callee = f[2] == b'1';
    let tf: i64 = w[4].parse().map_err(|e| anyhow!("{line:?}: {e}"))?;
    let entry = u64::from_str_radix(w[0], 16).map_err(|e| anyhow!("{line:?}: {e}"))?;
    let touched = match w.get(5) {
        Some(t) => RegSet(hex(t)?),
        None => RegSet::ALL,
    };
    let guardfree = w.get(6) == Some(&"1");
    Ok((entry, s, (tf >= 0).then_some(tf as u32), touched, guardfree))
}

/// Stage 1: write `funcs` (memory form, already [`crate::select`]ed) and
/// what codegen needs of `sums` to `dir`, atomically. `class32` is part of
/// the record: the emitted C depends on it.
pub fn write_recovery(dir: &Path, funcs: &[Function], sums: &HashMap<u64, Summary>, class32: bool) -> Result<()> {
    let trapfree = func::trap_free(funcs, sums);
    let touched = func::touched(funcs);
    let guardfree = func::guard_free(funcs, sums);
    let parent = dir.parent().ok_or_else(|| anyhow!("{} has no parent", dir.display()))?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        dir.file_name().and_then(|n| n.to_str()).unwrap_or("recovery"),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    {
        let mut meta = BufWriter::new(std::fs::File::create(tmp.join("summaries"))?);
        writeln!(meta, "{FORMAT}")?;
        writeln!(meta, "class32 {}", class32 as u8)?;
        for f in funcs {
            let s = sums.get(&f.entry).cloned().unwrap_or_else(Summary::unknown);
            let t = touched.get(&f.entry).copied().unwrap_or(RegSet::ALL);
            meta.write_all(summary_line(f.entry, &s, trapfree.get(&f.entry).copied(), t, guardfree.contains(&f.entry)).as_bytes())?;
        }
        meta.flush()?;
        let mut gir = BufWriter::new(std::fs::File::create(tmp.join("functions.gir"))?);
        writeln!(gir, "{FORMAT}")?;
        for f in funcs {
            let text = gex_ir::text::print(f);
            writeln!(gir, "## {:x} {}", f.entry, text.len())?;
            gir.write_all(text.as_bytes())?;
        }
        gir.flush()?;
    }
    let _ = std::fs::remove_dir_all(dir);
    std::fs::rename(&tmp, dir).with_context(|| format!("moving the recovery into {}", dir.display()))?;
    Ok(())
}

/// The per-function lines of a recovery: entry, summary, trap-free weight.
struct Recovery {
    class32: bool,
    entries: Vec<u64>,
    sums: HashMap<u64, Summary>,
    trapfree: HashMap<u64, u32>,
    touched: HashMap<u64, RegSet>,
    guardfree: HashSet<u64>,
}

fn read_format(r: &mut impl BufRead, what: &Path) -> Result<()> {
    let mut first = String::new();
    r.read_line(&mut first)?;
    if first.trim_end() != FORMAT {
        bail!(
            "{} is {:?}, not {FORMAT:?}: a stale or foreign recovery cache; delete it",
            what.display(),
            first.trim_end()
        );
    }
    Ok(())
}

fn read_recovery(dir: &Path) -> Result<Recovery> {
    let path = dir.join("summaries");
    let mut r = BufReader::new(std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?);
    read_format(&mut r, &path)?;
    let mut line = String::new();
    r.read_line(&mut line)?;
    let class32 = match line.trim_end() {
        "class32 0" => false,
        "class32 1" => true,
        other => bail!("{}: expected the class32 line, found {other:?}", path.display()),
    };
    let mut rec = Recovery { class32, entries: Vec::new(), sums: HashMap::new(), trapfree: HashMap::new(), touched: HashMap::new(), guardfree: HashSet::new() };
    for line in r.lines() {
        let (e, s, tf, t, g) = parse_summary_line(&line?)?;
        rec.touched.insert(e, t);
        if g {
            rec.guardfree.insert(e);
        }
        rec.entries.push(e);
        rec.sums.insert(e, s);
        if let Some(w) = tf {
            rec.trapfree.insert(e, w);
        }
    }
    Ok(rec)
}

/// The C a unit holds for the function at `entry`, from the cached recovery
/// in `recovery` (`gexc cgen-c`): what to read when comparing one function's
/// code with another backend's. `read` gives the image's bytes, for `ret N`.
pub fn function_c(recovery: &Path, entry: u64, read: &dyn Fn(u64) -> Option<u8>, icall_hints: &HashMap<u64, u64>) -> Result<String> {
    let rec = read_recovery(recovery)?;
    let known: HashSet<u64> = rec.entries.iter().copied().collect();
    let mut gir = GirReader::open(recovery)?;
    while let Some(f) = gir.next()? {
        if f.entry != entry {
            continue;
        }
        let mut unit = FnUnit::new();
        unit.set_class32(rec.class32);
        unit.ftab = true;
        unit.touched = rec.touched.clone();
        unit.guardfree = rec.guardfree.clone();
        unit.icall_hints = std::sync::Arc::new(icall_hints.clone());
        unit.ret_pops.extend(func::ret_pops(std::slice::from_ref(&f), rec.class32, read));
        unit.add(&f, &known, &rec.sums, &rec.trapfree)?;
        return Ok(unit.source());
    }
    bail!("no function at {entry:#x} in {}", recovery.display())
}

/// The functions of a recovery, parsed one at a time.
struct GirReader {
    r: BufReader<std::fs::File>,
    path: PathBuf,
}

impl GirReader {
    fn open(dir: &Path) -> Result<GirReader> {
        let path = dir.join("functions.gir");
        let mut r = BufReader::new(std::fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?);
        read_format(&mut r, &path)?;
        Ok(GirReader { r, path })
    }

    fn next(&mut self) -> Result<Option<Function>> {
        let mut head = String::new();
        if self.r.read_line(&mut head)? == 0 {
            return Ok(None);
        }
        let w: Vec<&str> = head.split_whitespace().collect();
        if w.len() != 3 || w[0] != "##" {
            bail!("{}: malformed record header {head:?}", self.path.display());
        }
        let len: usize = w[2].parse()?;
        let mut buf = vec![0u8; len];
        self.r.read_exact(&mut buf)?;
        let text = String::from_utf8(buf)?;
        let f = gex_ir::text::parse(&text).map_err(|e| anyhow!("{}: function {}: {e}", self.path.display(), w[1]))?;
        Ok(Some(f))
    }
}

/// A 128-bit content hash (two keyed SipHashes): object-cache names.
fn hash128(parts: &[&[u8]]) -> String {
    let mut out = String::new();
    for salt in [0x9e37_79b9u64, 0x85eb_ca6b] {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        salt.hash(&mut h);
        for p in parts {
            p.len().hash(&mut h);
            p.hash(&mut h);
        }
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

/// What `build_c_module` did, for the build's report.
#[derive(Debug, Default, Clone)]
pub struct ImageStats {
    pub functions: usize,
    pub units: usize,
    pub compiled: usize,
    pub cached: usize,
    pub c_bytes: u64,
    pub wasm_bytes: u64,
    /// Indirect calls with a guarded direct call (`FnUnit::icall_hints`).
    pub devirt: usize,
    /// Calls to unit-private copies of small leaves (`FnUnit::leaves`).
    pub leaf_copies: usize,
}

/// Stage 2: the recovery in `recovery` as one linked C module at `out`.
/// `work` holds the unit sources while they compile; `objects` is the object
/// cache (shared across images and builds). `read` gives a byte of the
/// image as placed (for [`func::ret_pops`]); `stubs` are entries an export
/// provider serves (`crate::provider::stubs`), whose bodies re-enter the
/// function table instead.
pub fn build_c_module(
    recovery: &Path,
    work: &Path,
    objects: &Path,
    out: &Path,
    jobs: usize,
    read: &(dyn Fn(u64) -> Option<u8> + Sync),
    stubs: &HashSet<u64>,
    icall_hints: &HashMap<u64, u64>,
) -> Result<ImageStats> {
    let rec = read_recovery(recovery)?;
    let hints = std::sync::Arc::new(icall_hints.clone());
    // A function clang compiles with a shadow-stack frame cannot be spliced:
    // it becomes a stub that leaves for tier-1 (`add_leave_stub`), its own
    // export goes, and only its unit recompiles. It must not be trap-free
    // (its callers would not have written everything back).
    // Export providers' addresses start as stubs (`crate::provider`).
    let mut skip: HashSet<u64> = stubs.clone();
    loop {
        let stats = emit_and_link(&rec, recovery, work, objects, out, jobs, &skip, read, &hints)?;
        let cwasm = std::fs::read(out)?;
        let users = crate::splice::unspliceable_users(&cwasm)?;
        if users.is_empty() {
            return Ok(stats);
        }
        let mut more = Vec::new();
        for n in &users {
            let a = func::guest_entry_of(n).ok_or_else(|| {
                anyhow!("C function {n} uses clang's shadow stack and belongs to no guest function")
            })?;
            if rec.trapfree.contains_key(&a) {
                bail!("{n} ({a:#x}) uses clang's shadow stack and is trap-free; it cannot be left to tier-1 here");
            }
            more.push(a);
        }
        if more.iter().all(|a| skip.contains(a)) {
            bail!("the shadow-stack users {users:?} remain after their functions became stubs");
        }
        eprintln!("[cgen] left to tier-1 (clang wanted a shadow stack): {more:x?}");
        skip.extend(more);
        let _ = std::fs::remove_file(out);
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_and_link(
    rec: &Recovery,
    recovery: &Path,
    work: &Path,
    objects: &Path,
    out: &Path,
    jobs: usize,
    skip: &HashSet<u64>,
    read: &(dyn Fn(u64) -> Option<u8> + Sync),
    hints: &std::sync::Arc<HashMap<u64, u64>>,
) -> Result<ImageStats> {
    std::fs::create_dir_all(work)?;
    // The small leaves every unit may copy (`FnUnit::leaves`), and how
    // their returns pop. A stub's body is not its function's: no copy.
    let mut leaves = HashMap::new();
    let mut leaf_pops = HashMap::new();
    if func::leaf_copies() {
        let mut gir = GirReader::open(recovery)?;
        while let Some(f) = gir.next()? {
            if !skip.contains(&f.entry) && func::is_small_copyable(&f) {
                leaf_pops.extend(func::ret_pops(std::slice::from_ref(&f), rec.class32, read));
                leaves.insert(f.entry, f);
            }
        }
    }
    let leaves = std::sync::Arc::new(leaves);
    let new_unit = || {
        let mut unit = FnUnit::new();
        unit.set_class32(rec.class32);
        unit.ftab = true;
        unit.touched = rec.touched.clone();
        unit.guardfree = rec.guardfree.clone();
        unit.icall_hints = std::sync::Arc::clone(hints);
        unit.leaves = std::sync::Arc::clone(&leaves);
        unit.ret_pops = leaf_pops.clone();
        unit
    };
    std::fs::create_dir_all(objects)?;
    let header_text = header::header();
    std::fs::write(work.join("gex_abi.h"), &header_text)?;
    let version = toolchain::clang_version()?;
    let flags = toolchain::cflags_key();
    // The precompiled header, made once per build directory.
    let pch = toolchain::precompile_header(work)?;
    let known: HashSet<u64> = rec.entries.iter().copied().collect();
    // The entries a host installs (a stub has no `e_`/`ib_`).
    let entries: Vec<u64> = rec.entries.iter().copied().filter(|a| !skip.contains(a)).collect();
    let mut stats = ImageStats { functions: rec.entries.len(), ..Default::default() };

    // Units go to a bounded queue of `jobs` compilers: emission waits when it
    // is full, so the C in flight stays at about `jobs` units.
    let (tx, rx) = std::sync::mpsc::sync_channel::<(PathBuf, PathBuf)>(jobs);
    let rx = std::sync::Mutex::new(rx);
    let errors = std::sync::Mutex::new(Vec::new());
    let mut objs: Vec<PathBuf> = Vec::new();
    std::thread::scope(|scope| -> Result<()> {
        for _ in 0..jobs.max(1) {
            scope.spawn(|| loop {
                let job = rx.lock().unwrap().recv();
                let Ok((src, obj)) = job else { return };
                if let Err(e) = toolchain::compile_one(work, &src, &obj, Some(&pch)) {
                    errors.lock().unwrap().push(e);
                }
                let _ = std::fs::remove_file(&src);
            });
        }
        let emit = |unit: &mut FnUnit, k: usize, objs: &mut Vec<PathBuf>, stats: &mut ImageStats| -> Result<()> {
            unit.add_leaf_copies(&known, &rec.sums, &rec.trapfree)?;
            let src_text = unit.source();
            stats.c_bytes += src_text.len() as u64;
            let key = hash128(&[src_text.as_bytes(), header_text.as_bytes(), flags.as_bytes(), version.as_bytes()]);
            let obj = objects.join(format!("{key}.o"));
            if obj.exists() {
                stats.cached += 1;
            } else {
                let src = work.join(format!("unit{k:06}.c"));
                std::fs::write(&src, &src_text)?;
                stats.compiled += 1;
                tx.send((src, obj.clone())).map_err(|_| anyhow!("the compilers stopped"))?;
            }
            objs.push(obj);
            stats.units += 1;
            stats.devirt += unit.stats.calls_devirt;
            stats.leaf_copies += unit.stats.calls_leaf_copy;
            *unit = new_unit();
            Ok(())
        };
        let mut gir = GirReader::open(recovery)?;
        let mut unit = new_unit();
        let mut k = 0usize;
        while let Some(f) = gir.next()? {
            if skip.contains(&f.entry) {
                unit.add_leave_stub(f.entry);
            } else {
                unit.ret_pops.extend(func::ret_pops(std::slice::from_ref(&f), rec.class32, read));
                unit.add(&f, &known, &rec.sums, &rec.trapfree)
                    .with_context(|| format!("emitting C for the function at {:#x}", f.entry))?;
            }
            if unit.len() >= UNIT_BYTES {
                emit(&mut unit, k, &mut objs, &mut stats)?;
                k += 1;
            }
            if !errors.lock().unwrap().is_empty() {
                break;
            }
        }
        if unit.len() > 0 {
            emit(&mut unit, k, &mut objs, &mut stats)?;
        }
        drop(tx);
        Ok(())
    })?;
    if let Some(e) = errors.into_inner().unwrap().into_iter().next() {
        return Err(e);
    }
    let tmp = out.with_extension("wasm.tmp");
    toolchain::link_image(work, &objs, &tmp)?;
    // Which guest function each slot of the element segment is, as the
    // `gex.funcs` section every function-table module carries, and beside
    // the module for a host that reads it without parsing wasm.
    let mut wasm = std::fs::read(&tmp)?;
    let slots = funcs_section(&wasm)?;
    let want: HashSet<u64> = entries.iter().copied().collect();
    let have: HashSet<u64> = slots.iter().copied().filter(|&a| a != 0).collect();
    if let Some(a) = want.difference(&have).next() {
        bail!("{a:#x}'s body is not in the unit's element segment");
    }
    append_funcs_section(&mut wasm, &slots);
    std::fs::write(&tmp, &wasm)?;
    stats.wasm_bytes = wasm.len() as u64;
    let mut eb = Vec::with_capacity(slots.len() * 8);
    for a in &slots {
        eb.extend_from_slice(&a.to_le_bytes());
    }
    std::fs::write(entries_path(out), eb)?;
    std::fs::rename(&tmp, out)?;
    Ok(stats)
}

/// The guest address of each slot a linked unit's element segment fills
/// (from `__table_base`): the function there, by its name (`f_<entry>`),
/// or 0 for anything else.
pub fn funcs_section(wasm: &[u8]) -> Result<Vec<u64>> {
    let mut slots: Vec<u32> = Vec::new();
    let mut names: HashMap<u32, u64> = HashMap::new();
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        match payload? {
            wasmparser::Payload::ElementSection(r) => {
                for el in r {
                    let el = el?;
                    if !slots.is_empty() {
                        bail!("the unit has more than one element segment");
                    }
                    if let wasmparser::ElementItems::Functions(fs) = el.items {
                        for f in fs {
                            slots.push(f?);
                        }
                    }
                }
            }
            wasmparser::Payload::CustomSection(c) if c.name() == "name" => {
                let reader = wasmparser::NameSectionReader::new(wasmparser::BinaryReader::new(c.data(), c.data_offset()));
                for n in reader.flatten() {
                    if let wasmparser::Name::Function(map) = n {
                        for naming in map.into_iter().flatten() {
                            if let Some(a) = naming.name.strip_prefix("f_").and_then(|h| u64::from_str_radix(h, 16).ok()) {
                                names.insert(naming.index, a);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if names.is_empty() {
        bail!("the unit has no function names (was it linked with names stripped?)");
    }
    Ok(slots.iter().map(|f| names.get(f).copied().unwrap_or(0)).collect())
}

/// Append the `gex.funcs` custom section (`gex_abi::ftab::FUNCS_SECTION`).
pub fn append_funcs_section(wasm: &mut Vec<u8>, slots: &[u64]) {
    let name = gex_abi::ftab::FUNCS_SECTION.as_bytes();
    let mut body = Vec::new();
    leb(&mut body, name.len() as u64);
    body.extend_from_slice(name);
    body.extend_from_slice(&(slots.len() as u32).to_le_bytes());
    for a in slots {
        body.extend_from_slice(&a.to_le_bytes());
    }
    wasm.push(0);
    leb(wasm, body.len() as u64);
    wasm.extend_from_slice(&body);
}

fn leb(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// Where a unit's entry list lives, beside its module.
pub fn entries_path(cwasm: &Path) -> std::path::PathBuf {
    cwasm.with_extension("entries")
}

/// The guest address of each slot of a unit built by [`build_c_module`],
/// in its element segment's order (0: not an entry).
pub fn read_entries(cwasm: &Path) -> Result<Vec<u64>> {
    let b = std::fs::read(entries_path(cwasm)).with_context(|| format!("reading {}", entries_path(cwasm).display()))?;
    Ok(b.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect())
}

/// The entries a C module from [`build_c_module`] exports, from its export
/// names (`e_<hex>`).
pub fn exported_entries(cwasm: &[u8]) -> Result<Vec<u64>> {
    let mut out = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(cwasm) {
        if let wasmparser::Payload::ExportSection(r) = payload? {
            for e in r {
                let e = e?;
                if let Some(a) = e.name.strip_prefix("e_").and_then(|h| u64::from_str_radix(h, 16).ok()) {
                    out.push(a);
                }
            }
        }
    }
    out.sort_unstable();
    Ok(out)
}

#[cfg(test)]
mod fingerprint_tests {
    /// Every `ctx::NAME` and `gex_abi::NAME` the code that writes a unit's
    /// C names must be in `abi_fingerprint`, or a gex-abi change that alters
    /// the C would not change the unit key.
    #[test]
    fn abi_fingerprint_covers_what_the_c_embeds() {
        let fp = super::abi_fingerprint();
        let sources = [
            ("func.rs", include_str!("func.rs")),
            ("emit.rs", include_str!("emit.rs")),
            ("image.rs", include_str!("image.rs").split("mod fingerprint_tests").next().unwrap()),
            ("lib.rs", include_str!("lib.rs")),
        ];
        // Not values: modules, types, functions of the host side.
        let ignore = ["ctx", "imports", "exports", "cache_op", "IMPORT_MODULE", "dispatch_entry_table", "dispatch_index"];
        let mut missing = Vec::new();
        for (file, text) in sources {
            for prefix in ["ctx::", "gex_abi::"] {
                for (i, _) in text.match_indices(prefix) {
                    let rest = &text[i + prefix.len()..];
                    let name: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
                    if name.is_empty() || ignore.contains(&name.as_str()) || name.chars().next().unwrap().is_ascii_lowercase() && name != "x" && name != "v" && name != "f" {
                        continue;
                    }
                    let key = if prefix == "ctx::" { format!("ctx::{name}") } else { name.clone() };
                    let listed = if ["x", "v", "f"].contains(&name.as_str()) {
                        fp.contains(&format!("ctx::{name}(0)="))
                    } else {
                        fp.contains(&format!("{key}=")) || fp.contains(&format!("GEX_{name} "))
                    };
                    if !listed && !missing.contains(&format!("{file}: {key}")) {
                        missing.push(format!("{file}: {key}"));
                    }
                }
            }
        }
        assert!(missing.is_empty(), "gex-abi values the C embeds but abi_fingerprint does not list: {missing:?}");
    }
}

#[cfg(test)]
mod abi_tests {
    /// Fails when a constant a unit embeds from gex-emit changes: bump
    /// `UNIT_ABI_VERSION` and record the new fingerprint here, so every unit
    /// built against the old constants is rebuilt.
    #[test]
    fn unit_abi_version_pins_the_embedded_constants() {
        assert_eq!(
            (super::UNIT_ABI_VERSION, super::unit_abi_fingerprint().as_str()),
            (1, "fma 134217729.0 523 1523")
        );
    }
}
