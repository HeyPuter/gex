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
    gex_key::unit_key(image, FORMAT, UNIT_INPUTS, UNIT_ABI_VERSION, RECOVERY_INPUTS)
}

/// The embedded constants [`UNIT_ABI_VERSION`] stands for.
pub fn unit_abi_fingerprint() -> String {
    format!("fma {:?} {} {}", gex_emit::fma::SPLIT, gex_emit::fma::SAFE_LO, gex_emit::fma::SAFE_HI)
}

/// C per translation unit before it is written and compiled.
pub const UNIT_BYTES: usize = 512 << 10;

/// What code generation reads of a function's summary (the rest stays
/// "unknown", which is always sound).
fn summary_line(entry: u64, s: &Summary, trapfree: Option<u32>) -> String {
    format!(
        "{entry:x} {:x} {:x} {}{}{} {}\n",
        s.inputs.0,
        s.preserved.0,
        s.may_unwind as u8,
        s.may_leave_pending as u8,
        s.unknown_callee as u8,
        trapfree.map(|w| w as i64).unwrap_or(-1)
    )
}

fn parse_summary_line(line: &str) -> Result<(u64, Summary, Option<u32>)> {
    let w: Vec<&str> = line.split_whitespace().collect();
    if w.len() != 5 || w[3].len() != 3 {
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
    Ok((entry, s, (tf >= 0).then_some(tf as u32)))
}

/// Stage 1: write `funcs` (memory form, already [`crate::select`]ed) and
/// what codegen needs of `sums` to `dir`, atomically. `class32` is part of
/// the record: the emitted C depends on it.
pub fn write_recovery(dir: &Path, funcs: &[Function], sums: &HashMap<u64, Summary>, class32: bool) -> Result<()> {
    let trapfree = func::trap_free(funcs, sums);
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
            meta.write_all(summary_line(f.entry, &s, trapfree.get(&f.entry).copied()).as_bytes())?;
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
    let mut rec = Recovery { class32, entries: Vec::new(), sums: HashMap::new(), trapfree: HashMap::new() };
    for line in r.lines() {
        let (e, s, tf) = parse_summary_line(&line?)?;
        rec.entries.push(e);
        rec.sums.insert(e, s);
        if let Some(w) = tf {
            rec.trapfree.insert(e, w);
        }
    }
    Ok(rec)
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
}

/// Stage 2: the recovery in `recovery` as one linked C module at `out`.
/// `work` holds the unit sources while they compile; `objects` is the object
/// cache (shared across images and builds).
pub fn build_c_module(recovery: &Path, work: &Path, objects: &Path, out: &Path, jobs: usize) -> Result<ImageStats> {
    let rec = read_recovery(recovery)?;
    // A function clang compiles with a shadow-stack frame cannot be spliced:
    // it becomes a stub that leaves for tier-1 (`add_leave_stub`), its own
    // export goes, and only its unit recompiles. It must not be trap-free
    // (its callers would not have written everything back).
    let mut skip: HashSet<u64> = HashSet::new();
    loop {
        let stats = emit_and_link(&rec, recovery, work, objects, out, jobs, &skip)?;
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
) -> Result<ImageStats> {
    std::fs::create_dir_all(work)?;
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
            *unit = FnUnit::new();
            unit.set_class32(rec.class32);
            unit.call_known_import = true;
            Ok(())
        };
        let mut gir = GirReader::open(recovery)?;
        let mut unit = FnUnit::new();
        unit.set_class32(rec.class32);
        unit.call_known_import = true;
        let mut k = 0usize;
        while let Some(f) = gir.next()? {
            if skip.contains(&f.entry) {
                unit.add_leave_stub(f.entry);
            } else {
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
        // The entries' table indices (`gex_fnref`): what the host reads
        // instead of an export per function.
        let fr = func::fnref_source(&entries);
        let key = hash128(&[fr.as_bytes(), header_text.as_bytes(), flags.as_bytes(), version.as_bytes()]);
        let obj = objects.join(format!("{key}.o"));
        if !obj.exists() {
            let src = work.join("fnref.c");
            std::fs::write(&src, fr)?;
            tx.send((src, obj.clone())).map_err(|_| anyhow!("the compilers stopped"))?;
        }
        objs.push(obj);
        drop(tx);
        Ok(())
    })?;
    if let Some(e) = errors.into_inner().unwrap().into_iter().next() {
        return Err(e);
    }
    let tmp = out.with_extension("wasm.tmp");
    toolchain::link_image(work, &objs, &tmp)?;
    stats.wasm_bytes = std::fs::metadata(&tmp)?.len();
    // The entries, in `gex_fnref`'s order, beside the module.
    let mut eb = Vec::with_capacity(entries.len() * 8);
    for a in &entries {
        eb.extend_from_slice(&a.to_le_bytes());
    }
    std::fs::write(entries_path(out), eb)?;
    std::fs::rename(&tmp, out)?;
    Ok(stats)
}

/// Where a unit's entry list lives, beside its module.
pub fn entries_path(cwasm: &Path) -> std::path::PathBuf {
    cwasm.with_extension("entries")
}

/// The entries of a unit built by [`build_c_module`], in `gex_fnref`'s
/// order.
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
