//! The function tier for a Windows program's images (PE units), and for
//! the ELF objects named for one.
//!
//! Wine's loader maps every image itself, so a Windows program's code
//! arrives through the runtime translator. With `gexc run --cgen` on a PE
//! program, every image the NT layer maps (the hook in
//! `gex_kernel::nt::section`) gets its function-tier unit, if one was built
//! for exactly those bytes at that base ([`prepare_pe`], `gexc cgen-image`,
//! `gexc cgen-prefix` for Wine's): the whole image's recovered functions as
//! C, linked once into a module of its own. Installing it instantiates that
//! module in every store against the process's body table and dispatcher and
//! writes each entry into the dispatch and body tables at its slot
//! (`publish_cgen_unit`, `install_cgen`): calls inside the image are wasm
//! calls, calls into any other installed image go through the body table,
//! and everything else -- code recovery missed, code generated at run time
//! -- stays the runtime translator's.
//!
//! A unit is keyed by the image as placed (its relocated bytes at its base)
//! and the build of gex: [`image_key`]. Before one is installed, the image's
//! executable bytes in guest memory are compared with what the unit was built
//! from; a mismatch stops the run, naming the first differing address. Code
//! rewritten afterwards (Detours) is caught when its page retires: its bit in
//! `gex_abi::PATCHED_PAGES` is set if the bytes differ ([`on_retire`]), and
//! compiled code leaves for a callee on such a page rather than call it. An
//! image without a unit runs on tier-1, said under `GEX_TRACE`; the
//! program's own exe without one stops the run, since `--cgen` was asked for.

use super::*;
use std::collections::HashMap;
use std::sync::OnceLock;

/// What a Windows program does about units (`gexc run`'s flags).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `--no-cgen`: tier-1 only.
    Off,
    /// The default: units where they exist; an image without one runs on
    /// tier-1 and gets one built in the background for the next run (units
    /// are only ever installed at map time, never swapped in mid-run).
    Default,
    /// `--cgen`: every program image (not Wine's) must have a unit.
    Require,
    /// An ejected project (docs/eject-pe.md): every image, Wine's included,
    /// has a unit built for its base ahead of time; nothing is ever built at
    /// run time, and an image without one stops the run.
    Aot,
}

static MODE: OnceLock<Mode> = OnceLock::new();

/// Set the mode before the program starts.
pub fn set_mode(m: Mode) {
    let _ = MODE.set(m);
}

pub(crate) fn mode() -> Mode {
    MODE.get().copied().unwrap_or(Mode::Default)
}

/// One unit build at a time per unit cache, across processes: a lock file
/// holding the builder's pid, taken over when that process is gone.
pub struct BuildLock(std::path::PathBuf);

impl Drop for BuildLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn build_lock() -> Result<BuildLock> {
    let root = cache_root()?;
    std::fs::create_dir_all(&root)?;
    let path = root.join("build.lock");
    let mut said = false;
    loop {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                use std::io::Write;
                let _ = write!(f, "{}", std::process::id());
                return Ok(BuildLock(path));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = std::fs::read_to_string(&path).unwrap_or_default();
                let alive = holder.trim().parse::<u32>().is_ok_and(|p| std::path::Path::new(&format!("/proc/{p}")).exists());
                if !alive && !holder.is_empty() {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                if !said {
                    eprintln!("[cgen] waiting for another build (pid {}) to finish", holder.trim());
                    said = true;
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
            Err(e) => return Err(anyhow!("{}: {e}", path.display())),
        }
    }
}

/// An image the NT layer mapped, waiting for the host.
#[derive(Clone)]
struct Pending {
    path: String,
    base: u64,
    pe: Arc<gex_ld::pe::PeImage>,
}

static PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());
/// Set after a push to [`PENDING`]: every syscall drains, and looks here
/// first rather than taking the lock.
static HAS_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static ON: OnceLock<()> = OnceLock::new();
/// One unit is placed at a time: slot assignment runs from a snapshot of the
/// page table to the publish.
static PLACING: Mutex<()> = Mutex::new(());

fn trace() -> bool {
    std::env::var_os("GEX_TRACE").is_some() || std::env::var_os("GEX_NT_CGEN_TRACE").is_some()
}

/// Where units live: `GEX_CGEN_CACHE`, else `<gex cache>/cgen`.
pub fn cache_root() -> Result<std::path::PathBuf> {
    if let Some(d) = std::env::var_os("GEX_CGEN_CACHE") {
        return Ok(d.into());
    }
    let modules = gex_cache::cache_dir().ok_or_else(|| anyhow!("no cache directory (set GEX_CGEN_CACHE)"))?;
    Ok(modules.parent().map(|p| p.join("cgen")).unwrap_or(modules))
}

/// The identity of an image's recovery, and of a unit: `gex_cgen::image`'s
/// (gex-key's, shared by every host so they cannot disagree).
pub fn recovery_key(image: &gex_ld::Image) -> String {
    gex_cgen::image::recovery_key(image)
}

pub fn image_key(image: &gex_ld::Image) -> String {
    gex_cgen::image::image_key(image)
}

/// The export providers for a PE image at `base` (`gex_cgen::provider`),
/// its exports by name, and the guest addresses they take over.
struct Provided {
    providers: Vec<gex_cgen::provider::Provider>,
    exports: HashMap<String, u32>,
    stubs: Vec<u64>,
    /// Those of `stubs` whose unit keeps a lifted twin
    /// (`gex_cgen::provider::twins`, marked `gex_abi::ftab::TWIN`).
    twins: Vec<u64>,
}

fn provided(pe: &gex_ld::pe::PeImage, base: u64) -> Provided {
    let providers = gex_cgen::provider::for_image(&pe.exports.dll_name, !pe.is64, &pe.data);
    let mut exports = HashMap::new();
    if !providers.is_empty() {
        for e in pe.exports.entries.iter().filter(|e| e.rva != 0 && e.forwarder.is_none()) {
            for n in &e.names {
                exports.insert(n.clone(), e.rva);
            }
        }
    }
    let stubs = gex_cgen::provider::stubs(&providers, &exports, base);
    let twins = gex_cgen::provider::twins(&providers, &exports, base);
    Provided { providers, exports, stubs, twins }
}

/// The key of `image`'s unit built around `pv`'s providers and its
/// indirect-call profile (`icall_hints`): the image's own key when there
/// are neither.
fn unit_key(image: &gex_ld::Image, pv: &Provided) -> String {
    use std::hash::{Hash, Hasher};
    let mut key = image_key(image);
    if !pv.stubs.is_empty() {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        pv.stubs.hash(&mut h);
        gex_cgen::provider::fingerprint(&pv.providers).hash(&mut h);
        key = format!("{key}-p{:016x}", h.finish());
    }
    let hints = icall_hints(image);
    if !hints.is_empty() {
        let mut v: Vec<(u64, u64)> = hints.into_iter().collect();
        v.sort_unstable();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        v.hash(&mut h);
        key = format!("{key}-i{:016x}", h.finish());
    }
    key
}

/// Where [`icall_hints`] reads the image's profile.
pub fn icall_path(image: &gex_ld::Image) -> Result<std::path::PathBuf> {
    Ok(cache_root()?.join("icall").join(format!("{}.txt", gex_key::image_identity(image).trim_start_matches("image:"))))
}

/// Say where the image's indirect-call profile was looked for and how many
/// hints it gave (`gexc cgen-image`). A unit built without one has no
/// devirtualised calls, so its hot virtual calls stay table calls that no
/// engine inlines (the browser units of 2026-10-04 were all built so). When
/// it is missing, each cache in `GEX_CGEN_ICALL_FROM` (colon-separated
/// cache roots) is checked for the same identity's file, and one found is
/// named, loudly.
fn report_hints(image: &gex_ld::Image) {
    let Ok(path) = icall_path(image) else { return };
    if path.exists() {
        eprintln!("[cgen] indirect-call hints: {} from {}", icall_hints(image).len(), path.display());
        return;
    }
    eprintln!("[cgen] indirect-call hints: none (no {})", path.display());
    let name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    for root in std::env::var("GEX_CGEN_ICALL_FROM").unwrap_or_default().split(':').filter(|r| !r.is_empty()) {
        let other = std::path::Path::new(root).join("icall").join(&name);
        if other.exists() {
            eprintln!(
                "[cgen] WARNING: building without the indirect-call profile at {}; copy it to {} for devirtualised calls",
                other.display(),
                path.display()
            );
        }
    }
}

/// The image's indirect-call profile, `<cache>/icall/<image identity>.txt`:
/// lines `icall <return address> <target> <count>` (pewasm's
/// `PEWASM_PROFILE_INDIRECT` dump, in this image's addresses; anything
/// before `icall` is ignored). As pewasm's `read_icall_hints`: a call site
/// whose dominant target takes at least 90% of at least 64 calls gets a
/// guarded direct call to it. By return address.
pub fn icall_hints(image: &gex_ld::Image) -> HashMap<u64, u64> {
    // The counting flavor devirtualises nothing (`image::icall_count`).
    if gex_cgen::image::icall_count() {
        return HashMap::new();
    }
    let Ok(path) = icall_path(image) else { return HashMap::new() };
    let Ok(text) = std::fs::read_to_string(&path) else { return HashMap::new() };
    let num = |w: &str| u64::from_str_radix(w.trim_start_matches("0x"), if w.starts_with("0x") { 16 } else { 10 }).ok();
    let mut sites: HashMap<u64, HashMap<u64, u64>> = HashMap::new();
    for l in text.lines() {
        let Some(k) = l.find("icall ") else { continue };
        let w: Vec<&str> = l[k + 6..].split_whitespace().collect();
        if let [r, t, n, ..] = w[..] {
            if let (Some(r), Some(t), Some(n)) = (num(r), num(t), num(n)) {
                *sites.entry(r).or_default().entry(t).or_default() += n;
            }
        }
    }
    let mut hints = HashMap::new();
    for (ret, targets) in sites {
        let total: u64 = targets.values().sum();
        let (t, best) = targets.into_iter().max_by_key(|&(t, n)| (n, std::cmp::Reverse(t))).unwrap();
        if best >= 64 && best * 10 >= total * 9 {
            hints.insert(ret, t);
        }
    }
    hints
}

/// Build (or find) the unit's C module for a PE image placed at `base`:
/// recovery once (cached), then C compiled unit by unit. Returns its path.
pub fn prepare_pe(pe: &gex_ld::pe::PeImage, base: u64, jobs: usize, verbose: bool) -> Result<std::path::PathBuf> {
    let (image, seeds) = gex_xlate::func::pe_input(pe, base).map_err(|e| anyhow!("placing the image at {base:#x}: {e}"))?;
    let pv = provided(pe, base);
    prepare_with(&image, &|| gex_xlate::func::recover_pe(&image, &seeds, &Default::default()), jobs, verbose, &pv)
}

/// Functions recovery found in the image at `base` that its unit (`unit`,
/// its `c.wasm`) has no entry for, its providers' stubs aside: those
/// gex-cgen left to tier-1 (clang wanted a shadow stack), which a runtime
/// with no translation cannot run (docs/eject-pe.md §3.4).
pub fn left_out(pe: &gex_ld::pe::PeImage, base: u64, unit: &std::path::Path) -> Result<Vec<u64>> {
    let (image, _) = gex_xlate::func::pe_input(pe, base).map_err(|e| anyhow!("placing the image at {base:#x}: {e}"))?;
    let summaries = cache_root()?.join("recovery").join(recovery_key(&image)).join("summaries");
    let Ok(text) = std::fs::read_to_string(&summaries) else { return Ok(Vec::new()) };
    let have: std::collections::HashSet<u64> = gex_cgen::image::read_entries(unit)?.into_iter().collect();
    let stubs = provided(pe, base).stubs;
    Ok(text
        .lines()
        .skip(2)
        .filter_map(|l| l.split_whitespace().next().and_then(|a| u64::from_str_radix(a, 16).ok()))
        .filter(|a| !have.contains(a) && stubs.binary_search(a).is_err())
        .collect())
}

/// [`prepare_pe`] with function starts of the caller's besides the image's
/// own (gex-eject: entries a recorded session reached that recovery did not
/// find). The unit has the same key: the caller owns the cache it builds
/// into, and starts from a cache without this image's recovery.
pub fn prepare_pe_seeded(pe: &gex_ld::pe::PeImage, base: u64, extra: &[u64], jobs: usize, verbose: bool) -> Result<std::path::PathBuf> {
    let (image, mut seeds) = gex_xlate::func::pe_input(pe, base).map_err(|e| anyhow!("placing the image at {base:#x}: {e}"))?;
    for &a in extra {
        if !seeds.starts.iter().any(|&(s, _)| s == a) {
            seeds.starts.push((a, gex_xlate::func::Source::Entry));
        }
    }
    let pv = provided(pe, base);
    prepare_with(&image, &|| gex_xlate::func::recover_pe(&image, &seeds, &Default::default()), jobs, verbose, &pv)
}

/// The recovery directory of a PE image at `base` (what [`prepare_pe`]
/// caches), for a caller that must recover it again.
pub fn recovery_dir(pe: &gex_ld::pe::PeImage, base: u64) -> Result<std::path::PathBuf> {
    let (image, _) = gex_xlate::func::pe_input(pe, base).map_err(|e| anyhow!("placing the image at {base:#x}: {e}"))?;
    Ok(cache_root()?.join("recovery").join(recovery_key(&image)))
}

/// The directory under the unit cache that holds the unit for a PE image
/// placed at `base` (with its providers and hints), built or not.
pub fn unit_dir(pe: &gex_ld::pe::PeImage, base: u64) -> Result<std::path::PathBuf> {
    let (image, _) = gex_xlate::func::pe_input(pe, base).map_err(|e| anyhow!("placing the image at {base:#x}: {e}"))?;
    let pv = provided(pe, base);
    Ok(cache_root()?.join(unit_key(&image, &pv)))
}

/// The guest addresses export providers take over in a PE image at `base`
/// (its unit has no entry there: the provider serves them).
pub fn provider_stubs(pe: &gex_ld::pe::PeImage, base: u64) -> Vec<u64> {
    provided(pe, base).stubs
}

/// A built-in provider's module and its slots: built here (clang) when
/// missing, unless this runtime has no translator (gex-rt), which only finds
/// the one its project's build made (`gex_cgen::provider::build`'s name).
pub(crate) fn provider_module(prov: &gex_cgen::provider::Provider) -> Result<(std::path::PathBuf, Vec<Option<String>>)> {
    if cfg!(feature = "translate") {
        return gex_cgen::provider::build(prov, &cache_root()?);
    }
    let path = cache_root()?
        .join("providers")
        .join(format!("{}-{}.wasm", prov.name, gex_cgen::provider::fingerprint(std::slice::from_ref(prov))));
    let wasm = std::fs::read(&path).with_context(|| format!("provider {} was not built with the project ({})", prov.name, path.display()))?;
    Ok((path, gex_cgen::provider::slot_names(&wasm)?))
}

/// Region entries (docs/eject-pe.md §3.3): functions entered at interior
/// addresses of an image -- a call continuation or a handler's resume point
/// inside another function, where control can arrive once the frames that
/// were running it are gone -- each running the rest of its code under the
/// same `F_body` contract. Built ahead of time (an ejected project records
/// which ones it needs) into a module of their own beside the image's unit,
/// at `<cache>/regions/<unit key>/`, and installed after it; outside the
/// unit's key, so the unit itself is unchanged.
fn regions_dir(unit: &std::path::Path) -> Option<std::path::PathBuf> {
    let key = unit.parent()?.file_name()?;
    Some(cache_root().ok()?.join("regions").join(key))
}

/// Build the region module for the image at `base` (whose unit is `unit`):
/// one function per address of `pcs` the unit has no entry for. Returns the
/// addresses it could not make a function of. Rebuilt only when `pcs`
/// change.
#[cfg(feature = "translate")]
pub fn prepare_regions(pe: &gex_ld::pe::PeImage, base: u64, unit: &std::path::Path, pcs: &[u64], jobs: usize) -> Result<Vec<u64>> {
    let pv = provided(pe, base);
    let (image, seeds) = gex_xlate::func::pe_input(pe, base).map_err(|e| anyhow!("placing the image at {base:#x}: {e}"))?;
    regions_from(&image, &seeds.starts, &pv.stubs, base, unit, pcs, jobs)
}

/// The NT trampoline page's region entries (at its unit `unit`): where a
/// host resumes it after a system call ([`gex_kernel::win::trampoline_resumes`]).
#[cfg(feature = "translate")]
pub fn prepare_trampoline_regions(class32: bool, unit: &std::path::Path, jobs: usize) -> Result<Vec<u64>> {
    let (image, seeds) = trampoline_input(class32);
    let pcs = gex_kernel::win::trampoline_resumes(class32);
    regions_from(&image, &seeds.starts, &[], gex_kernel::win::TRAMPOLINES, unit, &pcs, jobs)
}

#[cfg(feature = "translate")]
#[allow(clippy::too_many_arguments)]
fn regions_from(
    image: &gex_ld::Image,
    seed_starts: &[(u64, gex_xlate::func::Source)],
    stubs: &[u64],
    base: u64,
    unit: &std::path::Path,
    pcs: &[u64],
    jobs: usize,
) -> Result<Vec<u64>> {
    use gex_xlate::func::{self as func, RecoverOptions, Source};
    struct Pv<'a> {
        stubs: &'a [u64],
    }
    let pv = Pv { stubs };
    let dir = regions_dir(unit).ok_or_else(|| anyhow!("no cache for region entries"))?;
    let have: std::collections::HashSet<u64> = gex_cgen::image::read_entries(unit)?.into_iter().collect();
    let mut want: Vec<u64> = pcs.iter().copied().filter(|a| !have.contains(a) && pv.stubs.binary_search(a).is_err()).collect();
    want.sort_unstable();
    want.dedup();
    // v3: call continuations of the enclosing functions, and direct callees.
    let list = String::from("v4\n") + &want.iter().map(|a| format!("{a:#x}\n")).collect::<String>();
    if want.is_empty() {
        let _ = std::fs::remove_dir_all(&dir);
        return Ok(Vec::new());
    }
    if std::fs::read_to_string(dir.join("pcs.txt")).ok().as_deref() == Some(list.as_str()) && dir.join("c.wasm").exists() {
        let got: std::collections::HashSet<u64> = gex_cgen::image::read_entries(&dir.join("c.wasm"))?.into_iter().collect();
        return Ok(want.into_iter().filter(|a| !got.contains(a)).collect());
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let code = region_code(image);
    let (mut starts, _) = func::seeds(&code, None, false);
    for &(a, src) in seed_starts {
        if code.is_code(a) {
            starts.entry(a).or_default().add(src);
        }
    }
    // The unit's functions bound every region: reaching one is a call or a
    // tail call into the unit, as in the function it continues.
    for &e in &have {
        starts.entry(e).or_default().add(Source::Call);
    }
    let noreturn = std::collections::BTreeSet::new();
    // A recorded call continuation means the frame of the function it
    // continues was discarded once (an exception resumed past it), and from
    // then on that function's code runs from regions, whose frames the next
    // resume discards in turn. So every call continuation of such a function
    // gets a region too (the recording saw only the first: tier-1 ran the
    // rest). The function is the unit's nearest entry at or below the PC.
    let mut sorted: Vec<u64> = have.iter().copied().collect();
    sorted.sort_unstable();
    let mut enclosing: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    for &pc in &want {
        let i = sorted.partition_point(|&e| e <= pc);
        if i > 0 {
            enclosing.insert(sorted[i - 1]);
        }
    }
    let mut extra: Vec<u64> = Vec::new();
    for &e in &enclosing {
        let mut st = starts.clone();
        let f = func::recover_one(&code, e, &mut st, &noreturn, &RecoverOptions::default());
        let rets: Vec<u64> = f
            .blocks
            .iter()
            .filter_map(|b| match b.term {
                gex_ir::func::FTerm::Call { ret_pc, .. } => Some(ret_pc),
                _ => None,
            })
            .collect();
        if rets.iter().any(|r| want.binary_search(r).is_ok()) {
            extra.extend(rets.into_iter().filter(|r| !have.contains(r) && pv.stubs.binary_search(r).is_err()));
        }
    }
    let n_recorded = want.len();
    want.extend(extra);
    want.sort_unstable();
    want.dedup();
    if want.len() > n_recorded {
        eprintln!("[regions] {base:#x}: {n_recorded} recorded, {} with the call continuations of their functions", want.len());
    }
    let mut batch = std::collections::BTreeMap::new();
    let mut failed = Vec::new();
    // Closed under direct calls and tail calls: a callee the unit lacks
    // (code recovery reached only from here, as from a landing pad) is a
    // region function too, or the first call to it would stop the run.
    let mut queue: std::collections::VecDeque<u64> = want.iter().copied().collect();
    let mut seen: std::collections::HashSet<u64> = want.iter().copied().collect();
    while let Some(pc) = queue.pop_front() {
        let mut st = starts.clone();
        let f = func::recover_one(&code, pc, &mut st, &noreturn, &RecoverOptions::default());
        match f.verify() {
            Ok(()) => {
                for b in &f.blocks {
                    let t = match b.term {
                        gex_ir::func::FTerm::Call { callee: gex_ir::func::Callee::Direct(t), .. } => t,
                        gex_ir::func::FTerm::TailCall { callee: gex_ir::func::Callee::Direct(t) } => t,
                        _ => continue,
                    };
                    if !have.contains(&t) && code.is_code(t) && pv.stubs.binary_search(&t).is_err() && seen.insert(t) {
                        queue.push_back(t);
                    }
                }
                batch.insert(pc, f);
            }
            Err(e) => {
                eprintln!("[regions] {pc:#x}: recovered malformed ({e})");
                failed.push(pc);
            }
        }
    }
    if batch.len() + failed.len() > want.len() {
        eprintln!("[regions] {base:#x}: {} more reached by direct calls", batch.len() + failed.len() - want.len());
    }
    let sums = gex_xlate::summary::analyze_functions(&batch).map;
    let funcs = gex_cgen::select(batch.into_values().collect(), &HashMap::new(), &std::collections::HashSet::new())?;
    let recovery = dir.join("recovery");
    gex_cgen::image::write_recovery(&recovery, &funcs, &sums, image.class32)?;
    let byte = |a: u64| image.segments.iter().find_map(|s| a.checked_sub(s.vaddr).and_then(|o| s.bytes.get(o as usize)).copied());
    let out = dir.join("c.wasm");
    gex_cgen::image::build_c_module(&recovery, &dir.join("work"), &cache_root()?.join("objects"), &out, jobs, &byte, &Default::default(), &HashMap::new())?;
    let _ = std::fs::remove_dir_all(dir.join("work"));
    std::fs::write(dir.join("pcs.txt"), &list)?;
    let got: std::collections::HashSet<u64> = gex_cgen::image::read_entries(&out)?.into_iter().collect();
    // Only recorded PCs count as failures: a continuation added above that
    // select() refused is reported by the run if it is ever reached.
    let recorded: std::collections::HashSet<u64> = pcs.iter().copied().collect();
    failed.retain(|a| recorded.contains(a));
    let more: Vec<u64> = want.iter().copied().filter(|a| recorded.contains(a) && !got.contains(a) && !failed.contains(a)).collect();
    failed.extend(more);
    Ok(failed)
}

/// `image`'s code, decoded in the image's own mode: in a 32-bit image
/// `[disp32]` is absolute, not relative to the next instruction.
#[cfg(feature = "translate")]
fn region_code(image: &gex_ld::Image) -> gex_xlate::func::Code<'_> {
    if image.class32 != gex_xlate::arch::compat() {
        gex_xlate::set_compat(image.class32);
    }
    gex_xlate::func::Code::with_elf(image, None)
}

/// The region module beside the unit at `unit`, if one was built.
pub fn region_module(unit: &std::path::Path) -> Option<std::path::PathBuf> {
    regions_dir(unit).map(|d| d.join("c.wasm")).filter(|p| p.exists())
}

/// Install `p`'s region module, if its build made one, after its unit.
fn install_regions(
    p: &Pending,
    pl: &Placed,
    shared: &Arc<Mutex<JitShared>>,
    engine: &Engine,
    cache: Option<&(std::path::PathBuf, String)>,
    memory: &gex_mem::GuestMemory,
) -> Result<()> {
    let Some(dir) = regions_dir(&pl.path) else { return Ok(()) };
    let path = dir.join("c.wasm");
    let Ok(wasm) = std::fs::read(&path) else { return Ok(()) };
    let entries = gex_cgen::image::read_entries(&path)?;
    let module = compile(engine, cache, &path, &wasm).with_context(|| format!("compiling {}'s region entries", p.path))?;
    crate::ftab::publish(shared, memory, module, &entries, gex_kernel::ftab::Tier::Unit, Vec::new())?;
    eprintln!("gex-nt: region entries for {}: {}", p.path, entries.iter().filter(|&&a| a != 0).count());
    Ok(())
}

/// Build every export provider for a PE image (`gex_cgen::provider`) into the
#[cfg(feature = "translate")]
/// unit cache, as mapping it would; their names.
pub fn prepare_providers(pe: &gex_ld::pe::PeImage) -> Result<Vec<String>> {
    let mut v = Vec::new();
    for prov in gex_cgen::provider::for_image(&pe.exports.dll_name, !pe.is64, &pe.data) {
        let (path, _) = gex_cgen::provider::build(&prov, &cache_root()?)?;
        if let Some(dir) = gex_cache::precompiled_dir() {
            gex_cache::precompile(&crate::engine()?, &std::fs::read(&path)?, &dir)?;
        }
        v.push(prov.name.to_string());
    }
    Ok(v)
}

/// A link group's member: an image and where it is mapped.
pub struct GroupMember<'a> {
    pub path: &'a str,
    pub pe: &'a gex_ld::pe::PeImage,
    pub base: u64,
}

/// What [`prepare_group`] made.
pub struct GroupBuilt {
    /// `<cache>/groups/<key>/c.wasm`.
    pub module: std::path::PathBuf,
    /// The group's key (its directory's name).
    pub key: String,
    /// Each member's unit key, in member order: what a host matches.
    pub member_keys: Vec<String>,
    /// Call sites made direct by resolving the members' IATs.
    pub iat_hints: usize,
}

/// The import address table of `m`, resolved against the other members as
/// Wine's loader binds it (forwarders followed within the group): IAT slot
/// address -> the target's address. Imports from images outside the group
/// are left out.
fn resolve_iats(m: &GroupMember<'_>, members: &[GroupMember<'_>]) -> HashMap<u64, u64> {
    let by_name = |dll: &str| {
        let d = dll.to_ascii_lowercase();
        let d = if d.ends_with(".dll") || d.ends_with(".exe") { d } else { format!("{d}.dll") };
        // API sets, as Wine's loader resolves them (its apiset schema): the
        // CRT ones to ucrtbase, the core ones to kernelbase. A wrong guess
        // costs nothing: the call compares the slot's value first.
        let d = if d.starts_with("api-ms-win-crt-") {
            "ucrtbase.dll".to_string()
        } else if d.starts_with("api-ms-win-core-") {
            "kernelbase.dll".to_string()
        } else {
            d
        };
        members.iter().find(|x| x.pe.exports.dll_name.eq_ignore_ascii_case(&d) || x.path.rsplit('/').next().is_some_and(|n| n.eq_ignore_ascii_case(&d)))
    };
    let mut out = HashMap::new();
    for imp in &m.pe.imports {
        let Some(target) = by_name(&imp.dll) else { continue };
        for sym in &imp.symbols {
            let mut t = target;
            let mut want = sym.import.clone();
            // At most a few forwarders deep.
            for _ in 0..4 {
                let Some(e) = t.pe.exports.find(&want) else { break };
                match &e.forwarder {
                    None => {
                        out.insert(m.base + sym.iat_rva as u64, t.base + e.rva as u64);
                        break;
                    }
                    Some(f) => {
                        let Some((dll, name)) = f.split_once('.') else { break };
                        let Some(next) = by_name(dll) else { break };
                        t = next;
                        want = match name.strip_prefix('#').and_then(|o| o.parse().ok()) {
                            Some(o) => gex_ld::pe::ImportName::Ordinal(o),
                            None => gex_ld::pe::ImportName::Name { name: name.to_string(), hint: 0 },
                        };
                    }
                }
            }
        }
    }
    out
}

/// A link group (docs/eject-pe.md §6a): every member's functions in one
/// module, calls between members direct. Each member's recovery is its
/// unit's (built if missing: [`prepare_pe`]). An IAT call (`call [slot]`,
/// `FF 15 disp32` before its return address) whose slot binds a member's
/// function gets a guarded direct call, as a devirtualised indirect call
/// (`if (target == K) f_K() else the table`), so a hooked IAT still works.
#[cfg(feature = "translate")]
pub fn prepare_group(members: &[GroupMember<'_>], jobs: usize) -> Result<GroupBuilt> {
    use std::hash::{Hash, Hasher};
    let root = cache_root()?;
    let mut recoveries = Vec::new();
    let mut member_keys = Vec::new();
    let mut stubs: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut hints: HashMap<u64, u64> = HashMap::new();
    let mut images = Vec::new();
    // G2 (docs/eject-pe.md §6a): the members' stateless providers compiled
    // into the group, their addresses served by bodies of its own.
    let with_providers = std::env::var("GEX_GROUP_PROVIDERS").is_ok_and(|v| v == "1");
    // G3: ThinLTO across the group.
    let lto = std::env::var("GEX_GROUP_LTO").is_ok_and(|v| v == "1");
    let mut gp = gex_cgen::image::GroupProviders::default();
    for m in members {
        prepare_pe(m.pe, m.base, jobs, true).with_context(|| format!("the unit for {}", m.path))?;
        let (image, _) = gex_xlate::func::pe_input(m.pe, m.base).map_err(|e| anyhow!("placing {} at {:#x}: {e}", m.path, m.base))?;
        let pv = provided(m.pe, m.base);
        member_keys.push(unit_key(&image, &pv));
        recoveries.push(root.join("recovery").join(recovery_key(&image)));
        stubs.extend(pv.stubs.iter().copied());
        stubs.extend(pv.twins.iter().copied());
        if with_providers {
            // A stateful provider (data, a stack) stays a module of its own.
            for prov in pv.providers.iter().filter(|p| !p.stateful) {
                if !gp.sources.iter().any(|(n, _)| n == prov.name) {
                    gp.sources.push((prov.name.to_string(), gex_cgen::provider::group_source(prov)));
                }
                for (f, e) in &prov.binds {
                    if let Some(&rva) = pv.exports.get(e) {
                        gp.bodies.entry(m.base + rva as u64).or_insert_with(|| gex_cgen::provider::group_symbol(prov, f));
                    }
                }
            }
        }
        hints.extend(icall_hints(&image));
        images.push(image);
    }
    let byte = |a: u64| images.iter().find_map(|im| im.segments.iter().find_map(|s| s.byte(a)));
    // The members' entries: an IAT target must be one (a provider's stub is
    // not; nor is data).
    let mut known: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for r in &recoveries {
        let text = std::fs::read_to_string(r.join("summaries")).with_context(|| format!("{}", r.display()))?;
        known.extend(text.lines().skip(2).filter_map(|l| l.split_whitespace().next().and_then(|a| u64::from_str_radix(a, 16).ok())));
    }
    let mut iat_hints = 0usize;
    for m in members {
        let iat = resolve_iats(m, members);
        if iat.is_empty() {
            continue;
        }
        // Every `call dword [slot]` in the member's code sections.
        for sec in m.pe.sections.iter().filter(|s| s.characteristics & 0x2000_0000 != 0) {
            let lo = m.base + sec.rva as u64;
            let n = sec.vsize.max(sec.raw_size) as u64;
            let mut a = lo;
            while a + 6 <= lo + n {
                if byte(a) == Some(0xff) && byte(a + 1) == Some(0x15) {
                    let d = (0..4).map(|i| byte(a + 2 + i).unwrap_or(0) as u64).enumerate().fold(0u64, |acc, (i, b)| acc | b << (8 * i));
                    if let Some(&t) = iat.get(&d) {
                        if known.contains(&t) && (!stubs.contains(&t) || gp.bodies.contains_key(&t)) {
                            // A recorded profile's hint for the site wins.
                            if !hints.contains_key(&(a + 6)) {
                                hints.insert(a + 6, t);
                                iat_hints += 1;
                            }
                        }
                    }
                }
                a += 1;
            }
        }
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    "gex link group v1".hash(&mut h);
    member_keys.hash(&mut h);
    let mut v: Vec<(u64, u64)> = hints.iter().map(|(a, b)| (*a, *b)).collect();
    v.sort_unstable();
    v.hash(&mut h);
    if with_providers {
        "providers".hash(&mut h);
        gp.sources.hash(&mut h);
        let mut b: Vec<(&u64, &String)> = gp.bodies.iter().collect();
        b.sort_unstable();
        b.hash(&mut h);
    }
    if lto {
        "thinlto".hash(&mut h);
    }
    let key = format!("{:016x}", h.finish());
    let dir = root.join("groups").join(&key);
    let out = dir.join("c.wasm");
    if !out.exists() {
        let t0 = std::time::Instant::now();
        let work = dir.join("work");
        let recs: Vec<&std::path::Path> = recoveries.iter().map(|p| p.as_path()).collect();
        gex_cgen::toolchain::set_lto(lto);
        let stats = gex_cgen::image::build_group_module(&recs, &work, &root.join("objects"), &out, jobs, &byte, &stubs, &hints, &gp);
        gex_cgen::toolchain::set_lto(false);
        let stats = stats?;
        // GEX_CGEN_KEEP: keep the generated C (dir/work) to read.
        if std::env::var_os("GEX_CGEN_KEEP").is_none() {
            let _ = std::fs::remove_dir_all(&work);
        }
        std::fs::write(dir.join("members.txt"), member_keys.join("\n") + "\n")?;
        eprintln!(
            "[cgen] link group {key}: {} members, {} functions, {} units ({} compiled, {} cached), {} MiB wasm, {:.1}s; {} devirtualised calls ({iat_hints} from the IATs)",
            members.len(),
            stats.functions,
            stats.units,
            stats.compiled,
            stats.cached,
            stats.wasm_bytes >> 20,
            t0.elapsed().as_secs_f64(),
            stats.devirt
        );
    }
    Ok(GroupBuilt { module: out, key, member_keys, iat_hints })
}

/// [`prepare_pe`] for any recovery input (no providers).
pub fn prepare_image(image: &gex_ld::Image, seeds: &gex_xlate::func::PeSeeds, jobs: usize, verbose: bool) -> Result<std::path::PathBuf> {
    let none = Provided { providers: Vec::new(), exports: HashMap::new(), stubs: Vec::new(), twins: Vec::new() };
    prepare_with(image, &|| gex_xlate::func::recover_pe(image, seeds, &Default::default()), jobs, verbose, &none)
}

fn prepare_with(
    image: &gex_ld::Image,
    recover: &dyn Fn() -> gex_xlate::func::Program,
    jobs: usize,
    verbose: bool,
    pv: &Provided,
) -> Result<std::path::PathBuf> {
    if verbose {
        report_hints(image);
    }
    let key = unit_key(image, pv);
    let root = cache_root()?;
    let dir = root.join(&key);
    let out = dir.join("c.wasm");
    if out.exists() {
        return Ok(out);
    }
    if mode() == Mode::Aot || cfg!(not(feature = "translate")) {
        bail!("no function-tier unit {key} in {} (the ejected project was not built for this image; run ./build.sh)", root.display());
    }
    let recovery = root.join("recovery").join(recovery_key(image));
    if !recovery.join("summaries").exists() {
        let t0 = std::time::Instant::now();
        let prog = recover();
        let summaries = gex_xlate::summary::analyze(&prog).map;
        let n = prog.functions.len();
        let funcs = gex_cgen::select(
            prog.functions.into_values().collect(),
            &HashMap::new(),
            &std::collections::HashSet::new(),
        )?;
        gex_cgen::image::write_recovery(&recovery, &funcs, &summaries, image.class32)?;
        if verbose {
            eprintln!(
                "[cgen] recovered {n} functions ({} kept) in {:.1}s",
                funcs.len(),
                t0.elapsed().as_secs_f64()
            );
        }
    }
    let t0 = std::time::Instant::now();
    let work = dir.join("work");
    let byte = |a: u64| {
        image.segments.iter().find_map(|s| s.byte(a))
    };
    let stubs: std::collections::HashSet<u64> = pv.stubs.iter().chain(&pv.twins).copied().collect();
    let hints = icall_hints(image);
    let stats = gex_cgen::image::build_c_module(&recovery, &work, &root.join("objects"), &out, jobs, &byte, &stubs, &hints)?;
    // GEX_CGEN_KEEP: keep the generated C (dir/work) to read.
    if std::env::var_os("GEX_CGEN_KEEP").is_none() {
        let _ = std::fs::remove_dir_all(&work);
    }
    if verbose {
        eprintln!(
            "[cgen] {} functions, {} units ({} compiled, {} cached), {} MiB of C, {} MiB wasm, {:.1}s; {} devirtualised calls ({} hints), {} calls to leaf copies",
            stats.functions,
            stats.units,
            stats.compiled,
            stats.cached,
            stats.c_bytes >> 20,
            stats.wasm_bytes >> 20,
            t0.elapsed().as_secs_f64(),
            stats.devirt,
            hints.len(),
            stats.leaf_copies
        );
    }
    Ok(out)
}

/// Turn the function tier on for this process's images (`--cgen`).
/// `record`: a profile is being recorded (`--record`), which names each
/// image as it maps (`profile::note_pe`), on every tier.
pub(crate) fn enable(record: bool) -> Result<()> {
    // Export providers bind on every tier, units or not.
    if mode() == Mode::Off && !gex_cgen::provider::enabled() && !record {
        return Ok(());
    }
    if ON.set(()).is_err() {
        return Ok(());
    }
    gex_kernel::nt::section::set_image_mapped_hook(|m| {
        PENDING.lock().unwrap_or_else(|e| e.into_inner()).push(Pending {
            path: m.path.to_string(),
            base: m.base,
            pe: Arc::clone(m.pe),
        });
        HAS_PENDING.store(true, std::sync::atomic::Ordering::Release);
    });
    Ok(())
}

fn take_pending() -> Vec<Pending> {
    // A load first: every syscall of every thread comes here, and a swap
    // would write the shared line each time.
    if !HAS_PENDING.load(std::sync::atomic::Ordering::Relaxed) || !HAS_PENDING.swap(false, std::sync::atomic::Ordering::Acquire) {
        return Vec::new();
    }
    std::mem::take(&mut *PENDING.lock().unwrap_or_else(|e| e.into_inner()))
}

/// A unit found for an image, checked against what is mapped.
struct Placed {
    path: std::path::PathBuf,
    cwasm: Arc<Vec<u8>>,
    entries: Vec<u64>,
    /// A hash of each executable page as the unit was built from it.
    pages: Vec<(u64, u64)>,
}

/// A native wasm32 Wine DLL's skeleton (docs/wine-native.md §3.13, §3.14):
/// no x86 code of its own (its exports' bytes are `ud2` stubs bound to the
/// module), so no unit is wanted or built for it.
pub fn is_wasm_skeleton(pe: &gex_ld::pe::PeImage) -> bool {
    pe.sections.iter().any(|s| s.name == ".wasmref" || s.name == ".wasm")
}

/// What placing an image needs that depends only on the image's bytes and
/// base: its executable segments as relocated there, and its unit (path,
/// compiled bytes, entries) if one was built.
struct Prepared {
    segs: Vec<(u64, Vec<u8>)>,
    /// Where its unit is or would be, and the provider stubs its entries
    /// leave out.
    path: std::path::PathBuf,
    stubs: Vec<u64>,
    unit: Option<(std::path::PathBuf, Arc<Vec<u8>>, Vec<u64>)>,
}

/// [`Prepared`] for `p`, remembered for the host process by path, base and
/// a hash of the file: under W3 every Wine process maps the same images at
/// the same bases, and the unit key (a SHA-256 of the relocated image) was
/// a fifth of a session boot's CPU.
fn prepared(p: &Pending) -> Result<Arc<Prepared>> {
    use std::hash::Hasher;
    type Memo = Vec<((String, u64, usize, u64), Arc<Prepared>)>;
    static MEMO: Mutex<Memo> = Mutex::new(Vec::new());
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write(&p.pe.data);
    let id = (p.path.clone(), p.base, p.pe.data.len(), h.finish());
    let known = MEMO.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(i, _)| *i == id).map(|(_, pr)| Arc::clone(pr));
    if let Some(pr) = known {
        // A unit built since (`--cgen`'s synchronous build, a background
        // one): read it now.
        if pr.unit.is_none() && pr.path.exists() {
            let unit = read_unit(&pr.path, &pr.stubs)?;
            let fresh = Arc::new(Prepared { segs: pr.segs.clone(), path: pr.path.clone(), stubs: pr.stubs.clone(), unit });
            let mut m = MEMO.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(e) = m.iter_mut().find(|(i, _)| *i == id) {
                e.1 = Arc::clone(&fresh);
            }
            return Ok(fresh);
        }
        return Ok(pr);
    }
    let (image, _) = gex_xlate::func::pe_input(&p.pe, p.base)
        .map_err(|e| anyhow!("{}: placing the image at {:#x}: {e}", p.path, p.base))?;
    let pv = provided(&p.pe, p.base);
    let cwasm_path = cache_root()?.join(remembered_unit_key(&id, &image, &pv)?).join("c.wasm");
    let segs = image.segments.iter().filter(|s| s.executable).map(|s| (s.vaddr, s.bytes.clone())).collect();
    let unit = read_unit(&cwasm_path, &pv.stubs)?;
    let pr = Arc::new(Prepared { segs, path: cwasm_path, stubs: pv.stubs, unit });
    let mut m = MEMO.lock().unwrap_or_else(|e| e.into_inner());
    if m.len() >= 512 {
        m.remove(0);
    }
    m.push((id, Arc::clone(&pr)));
    Ok(pr)
}

/// [`unit_key`], remembered on disk across runs (`<cache>/w3keys`) by the
/// file's path, base, length and SipHash (`id`), this gexc binary and the
/// indirect-call profiles' directory: the key is two SHA-256s of the
/// relocated image, a third of wineboot's host CPU in a session boot.
fn remembered_unit_key(id: &(String, u64, usize, u64), image: &gex_ld::Image, pv: &Provided) -> Result<String> {
    use std::hash::{Hash, Hasher};
    let root = cache_root()?;
    let stamp = |p: &std::path::Path| {
        std::fs::metadata(p).ok().map(|m| (m.len(), m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos())))
    };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    "gex-w3-key 1".hash(&mut h);
    id.hash(&mut h);
    // What else decides the key at run time: the memory flavour, guards
    // and store widths (gex-cgen's unit inputs).
    (gex_abi::mem64(), gex_cgen::image::unguarded(), gex_cgen::image::narrow_stores()).hash(&mut h);
    std::env::current_exe().ok().and_then(|e| stamp(&e)).hash(&mut h);
    stamp(&root.join("icall")).hash(&mut h);
    // Which providers there are (sources from outside gex, the skip list).
    gex_cgen::provider::settings_fingerprint().hash(&mut h);
    let memo = root.join("w3keys").join(format!("{:016x}", h.finish()));
    if let Ok(k) = std::fs::read_to_string(&memo) {
        if !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Ok(k);
        }
    }
    let k = unit_key(image, pv);
    if std::fs::create_dir_all(root.join("w3keys")).is_ok() {
        let tmp = memo.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, &k).is_ok() {
            let _ = std::fs::rename(&tmp, &memo);
        }
    }
    Ok(k)
}

/// The unit at `path` (`.../c.wasm`), if it is there: its bytes and its
/// entries, a provider's stubs (`stubs`) left out.
fn read_unit(path: &std::path::Path, stubs: &[u64]) -> Result<Option<(std::path::PathBuf, Arc<Vec<u8>>, Vec<u64>)>> {
    let Ok(cwasm) = std::fs::read(path) else { return Ok(None) };
    let mut entries = gex_cgen::image::read_entries(path)?;
    // A provider's addresses are its stubs: the provider is bound there.
    for a in entries.iter_mut() {
        if stubs.binary_search(a).is_ok() {
            *a = 0;
        }
    }
    Ok(Some((path.to_path_buf(), Arc::new(cwasm), entries)))
}

/// A unit for `p`, if one was built, with the byte check.
/// `at_map`: Wine has only just mapped the image, and every byte counts, its
/// import slots too: a unit placed before the loader has filled them (and
/// put back the protection it lifts to do it, which retires the page and
/// the unit's functions on it) waits for the image's first miss instead.
fn place(p: &Pending, memory: &[u8], at_map: bool) -> Result<Option<Placed>> {
    if is_wasm_skeleton(&p.pe) {
        return Ok(None);
    }
    let pr = prepared(p)?;
    let Some((cwasm_path, cwasm, entries)) = pr.unit.clone() else {
        return Ok(None);
    };
    let mut pages = Vec::new();
    note_import_slots(p.base, &p.pe);
    // What the unit was built from must be what is mapped.
    for (vaddr, bytes) in &pr.segs {
        let (lo, n) = (*vaddr as usize, bytes.len());
        let live = memory
            .get(lo..lo + n)
            .ok_or_else(|| anyhow!("{}: {:#x}+{n:#x} is outside guest memory", p.path, vaddr))?;
        if let Some(i) = gex_xlate::pe_code_difference(*vaddr, bytes, live, p.base, (!at_map).then_some(&*p.pe)) {
            bail!(
                "{}: the mapped code at {:#x} differs from the image its function-tier unit was \
                 built from (guest {:#04x}, unit {:#04x}); refusing the unit",
                p.path,
                vaddr + i as u64,
                live[i],
                bytes[i]
            );
        }
        let first = vaddr / gex_abi::DISPATCH_PAGE;
        let last = (vaddr + n as u64 - 1) / gex_abi::DISPATCH_PAGE;
        for page in first..=last {
            pages.push((page, page_hash(memory, page)));
        }
    }
    if !entries.iter().any(|&a| a != 0) {
        bail!("{}: its function-tier unit exports no entries", p.path);
    }
    Ok(Some(Placed { path: cwasm_path, cwasm, entries, pages }))
}

/// A hash of guest page `page` as it is now.
fn page_hash(memory: &[u8], page: u64) -> u64 {
    let slots = IMPORT_SLOTS.lock().unwrap_or_else(|e| e.into_inner());
    gex_xlate::masked_page_hash(memory, page, gex_abi::DISPATCH_PAGE, &slots)
}

/// The import slots of every PE image placed (absolute, sorted): bytes the
/// loader writes that are not code ([`page_hash`]).
static IMPORT_SLOTS: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

fn note_import_slots(base: u64, pe: &gex_ld::pe::PeImage) {
    let end = base + pe.image_size as u64;
    {
        let mut v = IMPORT_SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        v.retain(|&(a, _)| a < base || a >= end);
        v.extend(gex_xlate::pe_import_slots(pe).into_iter().map(|(a, b)| (base + a, base + b)));
        v.sort_unstable();
    }
    let mut v = PE_IMAGES.lock().unwrap_or_else(|e| e.into_inner());
    v.retain(|&(a, b)| b <= base || a >= end);
    v.push((base, end));
}

/// Where the PE images with units are (start, end).
static PE_IMAGES: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

/// Whether `addr` is inside a PE image a unit was placed for: a page of one
/// made non-executable for a while (Wine's loader lifts `.text` to fill an
/// import table there, then puts it back) is not gone (the hosts'
/// `gone_code`). Its units stay; its bytes are compared when it is
/// executable again (the kernel retires it then), and a changed page reads
/// as patched.
pub(crate) fn in_pe_image(addr: u64) -> bool {
    PE_IMAGES.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|&(a, b)| a <= addr && addr < b)
}

/// The pages installed units cover, with their hash as built.
static UNIT_PAGES: Mutex<Option<HashMap<u64, u64>>> = Mutex::new(None);

/// Pages that have retired (`retire_translations`): any an installed unit
/// covers whose bytes are no longer the ones it was built from gets its bit
/// in `gex_abi::PATCHED_PAGES`, and compiled code stops calling into it.
///
/// `gone` (within `ranges`): pages no longer mapped executable. A unit there
/// is dead whatever the bytes still hash to -- an unmapped object's bytes stay
/// until the next mapping overwrites them, and that mapping retires nothing
/// (the browser's `cgen::on_retire`). An ELF object whose code was there is
/// placed again if it is mapped again.
pub(crate) fn on_retire(memory: &gex_mem::GuestMemory, ranges: &[(u64, u64)], gone: &[(u64, u64)]) {
    ELF_SEEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(_, _, (lo, hi))| !gone.iter().any(|&(s, e)| s < *hi && *lo < e));
    let memory = memory.bytes();
    let guard = UNIT_PAGES.lock().unwrap_or_else(|e| e.into_inner());
    let Some(pages) = guard.as_ref() else { return };
    for &(start, end) in ranges {
        for page in start / gex_abi::DISPATCH_PAGE..end.div_ceil(gex_abi::DISPATCH_PAGE) {
            let Some(&built) = pages.get(&page) else { continue };
            let at = page * gex_abi::DISPATCH_PAGE;
            let unmapped = gone.iter().any(|&(s, e)| s <= at && at < e);
            if unmapped || page_hash(memory, page) != built {
                let off = (gex_abi::patched_pages() + page / 8) as usize;
                if let Some(b) = memory.get_mut(off) {
                    *b |= 1 << (page % 8);
                }
                if trace() {
                    eprintln!("gex-nt: page {:#x} of a compiled image was rewritten: its functions leave for tier-1", page * gex_abi::DISPATCH_PAGE);
                }
            }
        }
    }
}

/// Whether an installed unit covers the code page of `pc`.
pub(crate) fn unit_covers(pc: u64) -> bool {
    UNIT_PAGES.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|m| m.contains_key(&(pc / gex_abi::DISPATCH_PAGE)))
}

/// Provided exports by guest address: a label (`kernel32.dll!OutputDebugStringA`),
/// the first bytes of the library's own code there when the provider was
/// bound, and whether a rewrite of them was reported.
static PROVIDED: Mutex<Option<std::collections::BTreeMap<u64, (String, [u8; PROVIDED_BYTES], bool)>>> = Mutex::new(None);
const PROVIDED_BYTES: usize = 8;

fn provided_bytes(memory: &[u8], addr: u64) -> [u8; PROVIDED_BYTES] {
    let mut b = [0; PROVIDED_BYTES];
    if let Some(m) = memory.get(addr as usize..addr as usize + PROVIDED_BYTES) {
        b.copy_from_slice(m);
    }
    b
}

fn note_provided(memory: &gex_mem::GuestMemory, addr: u64, label: String) {
    let bytes = provided_bytes(memory.bytes(), addr);
    PROVIDED.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert(addr, (label, bytes, false));
}

/// Report, once per export, a guest that rewrote the start of an export a
/// provider serves (a Detours-style hook, a patch). The provider stays
/// bound (`ftab::Shared::providers`), so the guest's new code there does
/// not run; on Windows it would. `GEX_PROVIDER_SKIP=<export>` leaves the
/// export to the library's code, hooks included.
pub(crate) fn provided_rewrites(memory: &gex_mem::GuestMemory, ranges: &[(u64, u64)]) {
    let memory = memory.bytes();
    let mut guard = PROVIDED.lock().unwrap_or_else(|e| e.into_inner());
    let Some(map) = guard.as_mut() else { return };
    for &(start, end) in ranges {
        for (&addr, (label, bytes, told)) in map.range_mut(start.saturating_sub(PROVIDED_BYTES as u64 - 1)..end) {
            if !*told && provided_bytes(memory, addr) != *bytes {
                *told = true;
                let e = label.rsplit('!').next().unwrap_or(label);
                eprintln!(
                    "gex-nt: the guest rewrote provided export {label} at {addr:#x} (a hook?); the provider stays bound, so its new code does not run (GEX_PROVIDER_SKIP={e} runs the library's own code)"
                );
            }
        }
    }
}

/// Record a placed unit's pages as built, and clear their patched bits.
fn note_pages(memory: &gex_mem::GuestMemory, pages: &[(u64, u64)]) {
    let memory = memory.bytes();
    let mut guard = UNIT_PAGES.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(Default::default);
    for &(page, h) in pages {
        map.insert(page, h);
        let off = (gex_abi::patched_pages() + page / 8) as usize;
        if let Some(b) = memory.get_mut(off) {
            *b &= !(1 << (page % 8));
        }
    }
}

/// The NT layer's trampoline page (`gex_kernel::win::trampoline_input`:
/// the unix-call and syscall dispatchers every Wine DLL calls through, Wine's
/// opengl32 once per GL call) as a recovery input. Fixed bytes at a fixed
/// address, so its unit is built once per gex build and installed at boot
/// like an image's: without it every such call leaves the function tier.
fn trampoline_input(class32: bool) -> (gex_ld::Image, gex_xlate::func::PeSeeds) {
    let (image, starts) = gex_kernel::win::trampoline_input(class32);
    let seeds = gex_xlate::func::PeSeeds {
        starts: starts.into_iter().map(|a| (a, gex_xlate::func::Source::Entry)).collect(),
        fdes: Vec::new(),
    };
    (image, seeds)
}

/// The NT trampoline page's unit, built into the cache (`gexc cgen-image
/// --nt-trampolines`): for a host that cannot build it at boot, the browser.
pub fn prepare_trampolines(class32: bool, jobs: usize) -> Result<std::path::PathBuf> {
    let (image, seeds) = trampoline_input(class32);
    let unit = prepare_image(&image, &seeds, jobs, true)?;
    // Its providers (nt_unixcall, i386 only), which a runtime without a
    // translator (gex-rt, the browser) finds in the cache and cannot build.
    if class32 {
        for prov in gex_cgen::provider::for_dll(gex_cgen::provider::TRAMPOLINES_DLL, true) {
            let (path, _) = gex_cgen::provider::build(&prov, &cache_root()?)?;
            #[cfg(feature = "translate")]
            if let Some(dir) = gex_cache::precompiled_dir() {
                gex_cache::precompile(&crate::engine()?, &std::fs::read(&path)?, &dir)?;
            }
            let _ = path;
        }
    }
    Ok(unit)
}

/// Build (once per gex build: cached) and publish the trampoline page's
/// unit and its providers, once per address space. Whether it did.
fn publish_trampolines(data: &HostState, class32: bool) -> Result<bool> {
    let jit = data.jit.as_ref().ok_or_else(|| anyhow!("runtime translation is not set up"))?;
    let (shared, engine, cache) = (Arc::clone(&jit.shared), jit.engine.clone(), jit.cache.clone());
    {
        let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
        if sh.trampolines {
            return Ok(false);
        }
        sh.trampolines = true;
    }
    let (image, seeds) = trampoline_input(class32);
    let t0 = std::time::Instant::now();
    let path = {
        let _lock = build_lock()?;
        prepare_image(&image, &seeds, 2, false)?
    };
    let cwasm = std::fs::read(&path)?;
    let memory = data.memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    for seg in &image.segments {
        let (lo, n) = (seg.vaddr as usize, seg.bytes.len());
        if memory.bytes().get(lo..lo + n) != Some(&seg.bytes[..]) {
            bail!("the trampoline page in guest memory is not the one its unit was built from");
        }
    }
    let entries = gex_cgen::image::read_entries(&path)?;
    let module = compile(&engine, cache.as_ref(), &path, &cwasm)?;
    crate::ftab::publish(&shared, &memory, module, &entries, gex_kernel::ftab::Tier::Unit, Vec::new())?;
    // Its providers (gex-cgen/providers/nt_unixcall.c), after the unit, at
    // the trampolines they replace.
    if class32 {
        use gex_kernel::win::{tramp, TRAMPOLINES};
        let exports = gex_cgen::provider::trampoline_exports((tramp::UNIX_CALL_32 - TRAMPOLINES) as u32);
        for prov in gex_cgen::provider::for_dll(gex_cgen::provider::TRAMPOLINES_DLL, true) {
            // As every provider: built here only where there is a translator
            // (gex-rt finds the project's), compiled through gex_cache (which
            // gex-rt serves from its precompiled artifacts).
            let (path, slots) = provider_module(&prov)?;
            let pairs = gex_cgen::provider::bindings(&prov, &slots, &exports, TRAMPOLINES, trace());
            if pairs.is_empty() {
                continue;
            }
            let module = std::fs::read(&path).map_err(anyhow::Error::from).and_then(|b| gex_cache::compile(&engine, &b)).with_context(|| format!("compiling provider {}", prov.name))?;
            crate::aot_share::note(&module, crate::aot_share::Kind::Provider);
            crate::ftab::publish_pairs(&shared, &memory, module, slots.len() as u32, &pairs, gex_kernel::ftab::Tier::Unit)?;
            if trace() {
                eprintln!("gex-cgen: provider {} for the trampolines: {} entries", prov.name, pairs.len());
            }
        }
    }
    if trace() {
        eprintln!("gex-cgen: function tier for the trampolines: {} entries, {:.1}s", entries.len(), t0.elapsed().as_secs_f64());
    }
    Ok(true)
}

/// [`publish_trampolines`] at boot, from the store.
fn install_trampolines(store: &mut Store<HostState>, class32: bool) -> Result<()> {
    if publish_trampolines(store.data(), class32)? {
        crate::ftab::catch_up(&mut *store)?;
        catch_up_table(store)?;
    }
    Ok(())
}

/// [`publish_trampolines`] for a wine32 or wine64 process, with its first
/// image: the page is in memory from the start (`run_program_with`), and
/// Wine's unix calls and syscalls all go through it.
fn w3_trampolines(caller: &mut Caller<'_, HostState>, class32: bool) -> Result<()> {
    if caller.data().jit.is_none() {
        return Ok(());
    }
    // Without it the page runs on tier-1, as before it had a unit here.
    match publish_trampolines(caller.data(), class32) {
        Ok(true) => {
            crate::ftab::catch_up(&mut *caller)?;
            replay_registry(caller)?;
        }
        Ok(false) => {}
        Err(e) => eprintln!("gex-w3: the trampoline page's unit: {e:#}; it runs on tier-1"),
    }
    Ok(())
}

/// Compile a unit's module. The compiled form is kept beside the unit
/// (`c.cwasm`), not in the shared module cache: a large image's is hundreds
/// of MiB, which that cache's LRU budget evicts at once, and Cranelift over
/// noita.exe's unit takes about a minute. Not keyed on the build of gex: the
/// unit directory is keyed on the module's content already, and wasmtime
/// refuses an artifact from an incompatible engine, which recompiles it.
/// The largest function body in a wasm module, in bytes.
fn largest_body(wasm: &[u8]) -> usize {
    let mut max = 0;
    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        if let Ok(wasmparser::Payload::CodeSectionEntry(body)) = payload {
            max = max.max(body.range().len());
        }
    }
    max
}

/// Units with a function body larger than this are compiled only at
/// staging (`GEX_CGEN_PRECOMPILE=1`), never at run time: Cranelift's
/// register allocation on d3dx9_43's 828 KB function takes about ten minutes
/// (wined3d's 444 KB one, seconds: size alone is a rough guide).
/// `GEX_W3_UNIT_MAX_BODY` (bytes) overrides.
fn max_runtime_body() -> usize {
    static M: OnceLock<usize> = OnceLock::new();
    *M.get_or_init(|| std::env::var("GEX_W3_UNIT_MAX_BODY").ok().and_then(|v| v.parse().ok()).unwrap_or(640 << 10))
}

/// Queue the unit at `path` (`wasm` its bytes) for [`precompile_unit`] on
/// one background thread of the host process, once; smallest first, so a
/// large unit does not hold up the rest.
fn compile_later(path: std::path::PathBuf, wasm: &[u8]) {
    struct Q {
        seen: std::collections::HashSet<std::path::PathBuf>,
        todo: Vec<(usize, std::path::PathBuf)>,
        worker: bool,
    }
    static QUEUE: OnceLock<(Mutex<Q>, std::sync::Condvar)> = OnceLock::new();
    let (q, cv) = QUEUE.get_or_init(|| (Mutex::new(Q { seen: Default::default(), todo: Vec::new(), worker: false }), std::sync::Condvar::new()));
    let mut g = q.lock().unwrap_or_else(|e| e.into_inner());
    if !g.seen.insert(path.clone()) {
        return;
    }
    let body = largest_body(wasm);
    if body > max_runtime_body() {
        eprintln!(
            "gex-w3: the unit {} has a {} KiB function; not compiled at run time (compile it at staging: \
             gexc cgen-image with GEX_CGEN_PRECOMPILE=1)",
            path.display(),
            body >> 10
        );
        return;
    }
    g.todo.push((wasm.len(), path));
    cv.notify_one();
    if !g.worker {
        g.worker = true;
        let _ = std::thread::Builder::new().name("gex-unit-cc".into()).spawn(move || loop {
            let (q, cv) = QUEUE.get().unwrap();
            let p = {
                let mut g = q.lock().unwrap_or_else(|e| e.into_inner());
                while g.todo.is_empty() {
                    g = cv.wait(g).unwrap_or_else(|e| e.into_inner());
                }
                let i = (0..g.todo.len()).min_by_key(|&i| g.todo[i].0).unwrap();
                g.todo.swap_remove(i).1
            };
            let t0 = std::time::Instant::now();
            match precompile_unit(&p) {
                Ok(()) => eprintln!("gex-w3: compiled the unit {} in {:.1}s (for the next process)", p.display(), t0.elapsed().as_secs_f64()),
                Err(e) => eprintln!("gex-w3: compiling the unit {}: {e:#}", p.display()),
            }
        });
    }
}

/// Compile the unit at `path` (`.../c.wasm`) for this host into its
/// `c.cwasm`, as its first install would.
pub fn precompile_unit(path: &std::path::Path) -> Result<()> {
    let wasm = std::fs::read(path)?;
    gex_cache::load_or_compile_at(&crate::engine()?, &path.with_extension("cwasm"), &wasm)?;
    Ok(())
}

fn compile(engine: &Engine, cache: Option<&(std::path::PathBuf, String)>, cwasm_path: &std::path::Path, cwasm: &[u8]) -> Result<Module> {
    if cache.is_none() {
        return Ok(gex_cache::compile(engine, cwasm)?);
    }
    gex_cache::load_or_compile_at(engine, &cwasm_path.with_extension("cwasm"), cwasm)
}

/// Build a unit for `p` in a background child for the next run: `gexc
/// cgen-image`, low priority, under a memory cap when `systemd-run` is there
/// (`GEX_CGEN_MEMORY_MAX`, default 12G). Builders take [`build_lock`], so
/// they run one at a time, and outlive this process.
fn spawn_build(p: &Pending) -> Result<std::process::Child> {
    // A process group of its own: the run ending -- a Ctrl-C at the terminal,
    // `timeout` signalling its group -- must not take the build with it.
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe()?;
    let max = std::env::var("GEX_CGEN_MEMORY_MAX").unwrap_or_else(|_| "12G".into());
    let base = format!("{:#x}", p.base);
    let mut args = vec!["cgen-image", p.path.as_str(), "--pe-base", base.as_str()];
    // The unit this process looks up is for its own guest memory.
    if gex_abi::mem64() {
        args.push("--mem64");
    }
    let capped = std::process::Command::new("systemd-run")
        .args(["--user", "--scope", "-q", "-p", &format!("MemoryMax={max}")])
        .arg("nice")
        .arg("-n")
        .arg("10")
        .arg(&exe)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .process_group(0)
        .spawn();
    match capped {
        Ok(c) => Ok(c),
        Err(_) => Ok(std::process::Command::new(&exe)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .process_group(0)
            .spawn()?),
    }
}

/// `GEX_NT_CGEN_SYNC=1` (tests/nt under --cgen): a program image mapped
/// without a unit has one built there and then, and installed, rather than
/// running on tier-1 or failing.
fn sync_build() -> bool {
    std::env::var_os("GEX_NT_CGEN_SYNC").is_some()
}

/// Build `p`'s unit now (the child `gexc cgen-image`), waiting for it.
fn build_now(p: &Pending) -> Result<()> {
    let status = spawn_build(p)?.wait()?;
    if !status.success() {
        bail!("building the function tier for {} failed ({status})", p.path);
    }
    Ok(())
}

/// A unit for `p`: placed if built; built first under `GEX_NT_CGEN_SYNC`.
fn place_or_build(p: &Pending, memory: &[u8]) -> Result<Option<Placed>> {
    if let Some(pl) = place(p, memory, true)? {
        return Ok(Some(pl));
    }
    if sync_build() && !is_wine(p) && mode() != Mode::Aot {
        build_now(p)?;
        return place(p, memory, true);
    }
    Ok(None)
}

/// An image Wine's prefix provides, not the program.
fn is_wine(p: &Pending) -> bool {
    p.path.to_ascii_lowercase().contains("/drive_c/windows/")
}

/// An image mapped with no unit for its base. `--cgen` refuses a program
/// image; otherwise it runs on tier-1 and, unless `GEX_NT_CGEN_BUILD` says
/// no (`0`, or a list of file names that leaves it out), gets a unit built
/// for the next run.
fn missing(p: Pending) -> Result<()> {
    if is_wasm_skeleton(&p.pe) {
        return Ok(());
    }
    if mode() == Mode::Aot {
        bail!(
            "gex-eject: {} was mapped at {:#x}, and the ejected project has no code built for it there \
             (the recorded session never mapped it, or mapped it elsewhere). Record a session that loads it \
             and re-eject",
            p.path,
            p.base
        );
    }
    // Always said, and kept for GEX_NT_STATS: an image on tier-1 because its
    // unit is missing (a base its unit was not built for, a stale key) is
    // otherwise only visible as a lower AOT share.
    let key = gex_xlate::func::pe_input(&p.pe, p.base)
        .map(|(image, _)| unit_key(&image, &provided(&p.pe, p.base)))
        .unwrap_or_else(|e| format!("(not placeable: {e})"));
    eprintln!("gex-nt: warning: {} at {:#x}: no function-tier unit (key {key}); it runs on tier-1", p.path, p.base);
    NO_UNIT.lock().unwrap_or_else(|e| e.into_inner()).push(format!("{} at {:#x} (key {key})", p.path, p.base));
    if mode() == Mode::Require && !is_wine(&p) {
        bail!(
            "--cgen: {} has no function-tier unit for base {:#x}; build one with \
             `gexc cgen-image {} --pe-base {:#x}`, or run once without --cgen (it is then built \
             for the next run)",
            p.path,
            p.base,
            p.path,
            p.base
        );
    }
    // Wine's DLLs get theirs at staging, at their layout bases (`gexc
    // cgen-prefix`); one placed anywhere else is not built for.
    let build = !is_wine(&p)
        && match std::env::var("GEX_NT_CGEN_BUILD").as_deref() {
        Err(_) | Ok("1") => true,
        Ok("0") => false,
        Ok(list) => {
            let name = p.path.rsplit(['/', '\\']).next().unwrap_or(&p.path);
            list.split(',').any(|n| n.trim().eq_ignore_ascii_case(name))
        }
        };
    if !build {
        if trace() {
            eprintln!("gex-nt: {} at {:#x}: no function-tier unit, tier-1", p.path, p.base);
        }
        return Ok(());
    }
    eprintln!("gex-nt: no function-tier unit for {} at {:#x}: tier-1 this run, building it for the next", p.path, p.base);
    match spawn_build(&p) {
        // Not waited for: it outlives the run, and the next one finds it.
        Ok(child) => drop(child),
        Err(e) => eprintln!("gex-nt: could not start building the function tier for {}: {e}", p.path),
    }
    Ok(())
}

/// Images mapped with no unit for their base, as `missing` reported them.
static NO_UNIT: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// GEX_NT_STATS's "no unit" line: every image running on tier-1 for want of
/// a unit, or `None` when there are none.
pub(crate) fn no_unit_line() -> Option<String> {
    let v = NO_UNIT.lock().unwrap_or_else(|e| e.into_inner());
    (!v.is_empty()).then(|| format!("gex-nt: no unit ({}): {}", v.len(), v.join(" | ")))
}

/// Place, compile, publish and install, for a caller inside the guest
/// (after an NT syscall that mapped an image).
pub(crate) fn drain_in_caller(caller: &mut Caller<'_, HostState>) -> Result<()> {
    if ON.get().is_none() {
        return Ok(());
    }
    let pending = take_pending();
    if pending.is_empty() {
        return Ok(());
    }
    let _one = PLACING.lock().unwrap_or_else(|e| e.into_inner());
    for p in pending {
        crate::profile::note_pe(&p.path, p.base, &p.pe);
        let (shared, engine, cache) = {
            let jit = caller.data().jit.as_ref().ok_or_else(|| anyhow!("runtime translation is not set up"))?;
            (Arc::clone(&jit.shared), jit.engine.clone(), jit.cache.clone())
        };
        let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
        let t0 = std::time::Instant::now();
        if mode() != Mode::Off {
            match place_or_build(&p, memory.bytes())? {
                Some(pl) => install_placed(&p, pl, &shared, &engine, cache.as_ref(), &memory, t0)?,
                None => missing(p.clone())?,
            }
        }
        install_providers(&p, caller.data_mut(), &shared, &engine, &memory)?;
        crate::ftab::catch_up(&mut *caller)?;
        crate::wasmdll::apply_pending(&shared, &memory);
        replay_registry(caller)?;
    }
    Ok(())
}

/// The same, at boot (the exe and ntdll, mapped before any guest code runs).
pub(crate) fn drain_in_store(store: &mut Store<HostState>) -> Result<()> {
    let pending = take_pending();
    if pending.is_empty() {
        return Ok(());
    }
    // The trampolines with the first images (the exe says the ABI).
    if let Some(p) = pending.first().filter(|_| mode() != Mode::Off) {
        install_trampolines(store, !p.pe.is64)?;
    }
    let _one = PLACING.lock().unwrap_or_else(|e| e.into_inner());
    for p in pending {
        crate::profile::note_pe(&p.path, p.base, &p.pe);
        let (shared, engine, cache) = {
            let jit = store.data().jit.as_ref().ok_or_else(|| anyhow!("runtime translation is not set up"))?;
            (Arc::clone(&jit.shared), jit.engine.clone(), jit.cache.clone())
        };
        let memory = store.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
        let t0 = std::time::Instant::now();
        if mode() != Mode::Off {
            match place_or_build(&p, memory.bytes())? {
                Some(pl) => install_placed(&p, pl, &shared, &engine, cache.as_ref(), &memory, t0)?,
                None => missing(p.clone())?,
            }
        }
        install_providers(&p, store.data_mut(), &shared, &engine, &memory)?;
        crate::ftab::catch_up(&mut *store)?;
        crate::wasmdll::apply_pending(&shared, &memory);
        catch_up_table(store)?;
    }
    Ok(())
}

/// A link group's state in this process: its members' unit keys, and the
/// members placed so far.
struct GroupState {
    key: String,
    members: Vec<String>,
    placed: Vec<(Pending, Placed)>,
    installed: bool,
}

/// The cache's link groups (`<cache>/groups/index.txt`: a group key, then its
/// members' unit keys, per line), read once.
static GROUPS: Mutex<Option<Vec<GroupState>>> = Mutex::new(None);

fn with_groups<R>(f: impl FnOnce(&mut Vec<GroupState>) -> R) -> R {
    let mut g = GROUPS.lock().unwrap_or_else(|e| e.into_inner());
    let v = g.get_or_insert_with(|| {
        let text = cache_root().ok().and_then(|r| std::fs::read_to_string(r.join("groups/index.txt")).ok()).unwrap_or_default();
        text.lines()
            .filter_map(|l| {
                let mut w = l.split_whitespace();
                let key = w.next()?.to_string();
                Some(GroupState { key, members: w.map(str::to_string).collect(), placed: Vec::new(), installed: false })
            })
            .collect()
    });
    f(v)
}

/// Publish a placed unit -- or, for a link group's member, hold it until
/// every member is placed, then publish the group's module in place of the
/// members' units (docs/eject-pe.md §6a). Wine maps an exe's static imports
/// before any of their code runs, so the group is in before a member runs.
#[allow(clippy::too_many_arguments)]
fn install_placed(
    p: &Pending,
    pl: Placed,
    shared: &Arc<Mutex<JitShared>>,
    engine: &Engine,
    cache: Option<&(std::path::PathBuf, String)>,
    memory: &gex_mem::GuestMemory,
    t0: std::time::Instant,
) -> Result<()> {
    let key = pl.path.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let member = with_groups(|gs| gs.iter().any(|g| !g.installed && g.members.contains(&key)));
    if !member {
        let module = compile(engine, cache, &pl.path, &pl.cwasm).with_context(|| format!("compiling {}'s function tier", p.path))?;
        note_pages(memory, &pl.pages);
        crate::ftab::publish(shared, memory, module, &pl.entries, gex_kernel::ftab::Tier::Unit, Vec::new())?;
        report(p, &pl, t0);
        return install_regions(p, &pl, shared, engine, cache, memory);
    }
    let ready = with_groups(|gs| {
        let g = gs.iter_mut().find(|g| !g.installed && g.members.contains(&key))?;
        g.placed.push((p.clone(), pl));
        if g.placed.len() < g.members.len() {
            return Some(None);
        }
        g.installed = true;
        Some(Some((g.key.clone(), std::mem::take(&mut g.placed))))
    });
    match ready.flatten() {
        None => {
            eprintln!("gex-nt: {} at {:#x}: held for its link group", p.path, p.base);
            Ok(())
        }
        Some((gkey, members)) => {
            let path = cache_root()?.join("groups").join(&gkey).join("c.wasm");
            let cwasm = std::fs::read(&path).with_context(|| format!("reading link group {}", path.display()))?;
            let module = compile(engine, cache, &path, &cwasm).with_context(|| format!("compiling link group {gkey}"))?;
            let mut entries = gex_cgen::image::read_entries(&path)?;
            // The members' providers' stubs: bound by the providers.
            let mut stubs: Vec<u64> = members.iter().flat_map(|(mp, _)| provided(&mp.pe, mp.base).stubs).collect();
            stubs.sort_unstable();
            for a in entries.iter_mut() {
                if stubs.binary_search(a).is_ok() {
                    *a = 0;
                }
            }
            for (_, mpl) in &members {
                note_pages(memory, &mpl.pages);
            }
            crate::ftab::publish(shared, memory, module, &entries, gex_kernel::ftab::Tier::Unit, Vec::new())?;
            eprintln!(
                "gex-nt: link group {gkey}: {} members ({}), {} entries, {} MiB wasm, placed in {:.1}s",
                members.len(),
                members.iter().map(|(mp, _)| mp.path.rsplit(['/', '\\']).next().unwrap_or("")).collect::<Vec<_>>().join(", "),
                entries.iter().filter(|&&a| a != 0).count(),
                cwasm.len() >> 20,
                t0.elapsed().as_secs_f64()
            );
            for (mp, mpl) in &members {
                install_regions(mp, mpl, shared, engine, cache, memory)?;
            }
            Ok(())
        }
    }
}

/// Bind `p`'s export providers (`gex_cgen::provider`) at their exports'"""
/// addresses, after its unit (whose bodies there are stubs into the table).
fn install_providers(p: &Pending, state: &mut HostState, shared: &Arc<Mutex<JitShared>>, engine: &Engine, memory: &gex_mem::GuestMemory) -> Result<()> {
    let pv = provided(&p.pe, p.base);
    // A native wasm32 Wine DLL's exports are its own module's (below).
    let native = is_wasm_skeleton(&p.pe);
    for prov in pv.providers.iter().filter(|_| !native) {
        // Small, and built in a private directory then renamed into place:
        // no need for the cache's build lock (which a unit build holds for
        // an hour).
        let (path, slots) = provider_module(prov)?;
        let mut pairs = gex_cgen::provider::bindings(prov, &slots, &pv.exports, p.base, trace());
        if prov.lifted {
            // Only where its unit keeps the lifted twin it falls back to:
            // none on tier-1 alone (--no-cgen, no unit for this base).
            let had = pairs.len();
            {
                let s = shared.lock().unwrap_or_else(|e| e.into_inner());
                pairs.retain(|&(a, _)| s.ftab.map.twin(a).is_some());
            }
            if pairs.len() < had {
                eprintln!("gex-nt: provider {} for {}: {} of {had} exports left unbound (no lifted twin in its unit)", prov.name, p.path, had - pairs.len());
            }
        }
        if pairs.is_empty() {
            continue;
        }
        let module = std::fs::read(&path).map_err(anyhow::Error::from).and_then(|b| gex_cache::compile(engine, &b)).with_context(|| format!("compiling provider {}", prov.name))?;
        crate::aot_share::note(&module, crate::aot_share::Kind::Provider);
        if prov.stateful {
            // Its data in guest pages, as an ejected project's stateful
            // providers have it (`libprov`); the stack comes per thread.
            let wasm = std::fs::read(&path)?;
            let (size, align) = crate::libprov::dylink_memory(&wasm)?;
            let a = 1u64 << align;
            let data = crate::libprov::alloc_top(state, (size as u64).max(16) + a, &format!("[provider {} data]", prov.name))?;
            let data = (data + a - 1) & !(a - 1);
            crate::ftab::publish_stateful(shared, memory, module, slots.len() as u32, &pairs, crate::libprov::Extras { memory_base: data })?;
        } else {
            crate::ftab::publish_pairs(shared, memory, module, slots.len() as u32, &pairs, gex_kernel::ftab::Tier::Unit)?;
        }
        let dll = p.path.rsplit(['/', '\\']).next().unwrap_or(&p.path);
        for (_, e) in &prov.binds {
            if let Some(&rva) = pv.exports.get(e) {
                note_provided(memory, p.base + rva as u64, format!("{dll}!{e}"));
            }
        }
        if crate::log_on() {
            eprintln!("gex-nt: provider {} for {}: {} exports", prov.name, p.path, pairs.len());
        }
    }
    // A native wasm32 Wine DLL (stream W): its module, at its skeleton's
    // exports (`wasmdll`).
    if let Some(n) = crate::wasmdll::install(&p.pe, &p.path, p.base, state, shared, engine, memory)? {
        crate::note_library(&p.path);
        if crate::log_on() {
            eprintln!("gex-nt: native wasm32 module for {}: {n} entries", p.path);
        }
    }
    // Stateful providers (`libprov`: an ejected project's), after the
    // built-in ones, so they win where both bind an export.
    let libs = crate::libprov::for_dll(&p.pe.exports.dll_name);
    if !libs.is_empty() {
        let mut exports = HashMap::new();
        for e in p.pe.exports.entries.iter().filter(|e| e.rva != 0 && e.forwarder.is_none()) {
            for n in &e.names {
                exports.insert(n.clone(), e.rva);
            }
        }
        for lp in libs {
            let n = crate::libprov::install(lp, state, shared, engine, memory, &exports, p.base, p.pe.entry_rva)?;
            eprintln!("gex-nt: provider {} for {}: {n} exports", lp.name, p.path);
        }
    }
    Ok(())
}

fn report(p: &Pending, pl: &Placed, t0: std::time::Instant) {
    // Once per image per process: debug output (GEX_LOG).
    if !crate::log_on() {
        return;
    }
    eprintln!(
        "gex-nt: function tier for {} at {:#x}: {} entries, {} MiB wasm, placed in {:.1}s",
        p.path,
        p.base,
        pl.entries.iter().filter(|&&a| a != 0).count(),
        pl.cwasm.len() >> 20,
        t0.elapsed().as_secs_f64()
    );
}

// ---------------------------------------------------------------------------
// Linux shared objects (stream U; docs/unity-dotnet.md §12)
// ---------------------------------------------------------------------------
//
// A Linux program's libraries are mapped by ld.so out of guest code, so they
// arrive through the runtime translator. `GEX_ELF_UNITS=name.so,...` (file
// names, or `all`) gives each listed object its function-tier unit at the base
// it was mapped at, as the NT layer's images get theirs: the whole object
// recovered (`gex_xlate::func::recover`, with its `.eh_frame` and symbols), as
// C, linked once (`gexc cgen-image --elf SO --base HEX`). A unit is keyed by
// the object's relocated bytes at that base, so the object must land at the
// same address every run (a `--pin` manifest line `BASE SPAN NAME` holds it
// there). An object with no unit runs on the fjit/tier-1 and, unless
// `GEX_ELF_UNITS_BUILD=0`, gets one built in the background for the next run.

/// The ELF object `bytes` placed at load bias `base`, with its own RELATIVE
/// relocations applied (code pointers in data, jump tables), as recovery sees it.
pub fn elf_image(bytes: &[u8], base: u64) -> Result<gex_ld::Image> {
    let mut image = gex_ld::elf::load(bytes, base).map_err(|e| anyhow!("loading the ELF object at {base:#x}: {e:?}"))?;
    gex_ld::elf::apply_own_relative(&mut image);
    Ok(image)
}

/// Build (or find) the unit for the ELF object `bytes` at `base`.
pub fn prepare_elf(bytes: &[u8], base: u64, jobs: usize, verbose: bool) -> Result<std::path::PathBuf> {
    let image = elf_image(bytes, base)?;
    let none = Provided { providers: Vec::new(), exports: HashMap::new(), stubs: Vec::new(), twins: Vec::new() };
    prepare_with(&image, &|| gex_xlate::func::recover(&image, Some(bytes), &Default::default()), jobs, verbose, &none)
}

/// The load bias of an ELF object one of whose executable `PT_LOAD`s was
/// mapped at `start` from file offset `offset`.
fn elf_bias(bytes: &[u8], start: u64, offset: u64) -> Option<u64> {
    let rd = |o: usize, n: usize| -> Option<u64> {
        let b = bytes.get(o..o + n)?;
        let mut v = [0u8; 8];
        v[..n].copy_from_slice(b);
        Some(u64::from_le_bytes(v))
    };
    if bytes.get(0..4)? != b"\x7fELF" || *bytes.get(4)? != 2 {
        return None;
    }
    let (phoff, phentsize, phnum) = (rd(0x20, 8)? as usize, rd(0x36, 2)? as usize, rd(0x38, 2)? as usize);
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        let (ty, flags, off, vaddr) = (rd(ph, 4)?, rd(ph + 4, 4)?, rd(ph + 8, 8)?, rd(ph + 16, 8)?);
        if ty == 1 && flags & 1 != 0 && off & !0xfff == offset {
            return Some(start.wrapping_sub(vaddr & !0xfff));
        }
    }
    None
}

fn elf_wanted(path: &str) -> bool {
    static LIST: OnceLock<Option<Vec<String>>> = OnceLock::new();
    let list = LIST.get_or_init(|| {
        std::env::var("GEX_ELF_UNITS").ok().filter(|v| !v.is_empty() && v != "0").map(|v| v.split(',').map(|s| s.trim().to_string()).collect())
    });
    let Some(list) = list else { return false };
    let name = path.rsplit('/').next().unwrap_or(path);
    list.iter().any(|n| n == "all" || n == name)
}

/// Objects already looked at in this host process, by path and bias.
static ELF_SEEN: Mutex<Vec<(String, u64, (u64, u64))>> = Mutex::new(Vec::new());

/// After a syscall that mapped code: install the unit of each listed ELF
/// object (`GEX_ELF_UNITS`), before ld.so runs any of it.
pub(crate) fn on_elf_maps(caller: &mut Caller<'_, HostState>, mapped: &[gex_kernel::CodeMap]) -> Result<()> {
    for m in mapped {
        if !elf_wanted(&m.path) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&m.path) else {
            eprintln!("gex-elf: {}: not readable on the host; no unit", m.path);
            continue;
        };
        let Some(base) = elf_bias(&bytes, m.start, m.offset) else {
            continue;
        };
        if ELF_SEEN.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|(p, b, _)| *p == m.path && *b == base) {
            continue;
        }
        let t0 = std::time::Instant::now();
        let image = elf_image(&bytes, base)?;
        // The executable span, so a retirement of that code forgets the object (`on_retire`).
        let span = image.segments.iter().filter(|s| s.executable).fold((u64::MAX, 0u64), |(lo, hi), s| {
            (lo.min(s.vaddr), hi.max(s.vaddr + s.bytes.len() as u64))
        });
        ELF_SEEN.lock().unwrap_or_else(|e| e.into_inner()).push((m.path.clone(), base, span));
        let none = Provided { providers: Vec::new(), exports: HashMap::new(), stubs: Vec::new(), twins: Vec::new() };
        let key = unit_key(&image, &none);
        let cwasm_path = cache_root()?.join(&key).join("c.wasm");
        let Ok(cwasm) = std::fs::read(&cwasm_path) else {
            eprintln!("gex-elf: no function-tier unit for {} at {base:#x} (key {key}); it runs on the runtime translator", m.path);
            NO_UNIT.lock().unwrap_or_else(|e| e.into_inner()).push(format!("{} at {base:#x} (key {key})", m.path));
            if std::env::var("GEX_ELF_UNITS_BUILD").as_deref() != Ok("0") {
                spawn_elf_build(&m.path, base);
            }
            continue;
        };
        let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
        // Only the executable segment this mapping holds is in memory yet; the
        // rest (another PT_LOAD) is checked when its own mapping arrives... so
        // compare what is there now and hash the pages of every executable segment.
        let mut pages = Vec::new();
        for seg in image.segments.iter().filter(|s| s.executable) {
            let (lo, n) = (seg.vaddr as usize, seg.bytes.len());
            let Some(live) = memory.bytes().get(lo..lo + n) else {
                bail!("gex-elf: {}: {:#x}+{n:#x} is outside guest memory", m.path, seg.vaddr);
            };
            if seg.vaddr >= m.start && seg.vaddr + n as u64 <= m.end {
                if let Some(i) = (0..n).find(|&i| live[i] != seg.bytes[i]) {
                    eprintln!(
                        "gex-elf: {}: the mapped code at {:#x} differs from the object its unit was built from; refusing the unit",
                        m.path,
                        seg.vaddr + i as u64
                    );
                    pages.clear();
                    break;
                }
            }
            for page in seg.vaddr / gex_abi::DISPATCH_PAGE..=(seg.vaddr + n as u64 - 1) / gex_abi::DISPATCH_PAGE {
                pages.push((page, page_hash(memory.bytes(), page)));
            }
        }
        if pages.is_empty() {
            continue;
        }
        let entries = gex_cgen::image::read_entries(&cwasm_path)?;
        let (shared, engine, cache) = {
            let jit = caller.data().jit.as_ref().ok_or_else(|| anyhow!("runtime translation is not set up"))?;
            (Arc::clone(&jit.shared), jit.engine.clone(), jit.cache.clone())
        };
        let _one = PLACING.lock().unwrap_or_else(|e| e.into_inner());
        let module = compile(&engine, cache.as_ref(), &cwasm_path, &cwasm).with_context(|| format!("compiling {}'s function tier", m.path))?;
        note_pages(&memory, &pages);
        crate::ftab::publish(&shared, &memory, module, &entries, gex_kernel::ftab::Tier::Unit, Vec::new())?;
        crate::ftab::catch_up(&mut *caller)?;
        replay_registry(caller)?;
        // Once per object per process: debug output (GEX_LOG).
        if crate::log_on() {
            eprintln!(
                "gex-elf: function tier for {} at {base:#x}: {} entries, {} MiB wasm, placed in {:.1}s",
                m.path,
                entries.iter().filter(|&&a| a != 0).count(),
                cwasm.len() >> 20,
                t0.elapsed().as_secs_f64()
            );
        }
    }
    Ok(())
}

/// `gexc cgen-image --elf PATH --base BASE` in the background, for the next run.
fn spawn_elf_build(path: &str, base: u64) {
    use std::os::unix::process::CommandExt;
    let Ok(exe) = std::env::current_exe() else { return };
    let mut cmd = std::process::Command::new("nice");
    cmd.arg("-n").arg("10").arg(&exe).args(["cgen-image", "--elf", path, "--base", &format!("{base:#x}")]);
    if gex_abi::mem64() {
        cmd.arg("--mem64");
    }
    if let Ok(j) = std::env::var("GEX_ELF_UNITS_JOBS") {
        cmd.args(["--jobs", &j]);
    }
    match cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).process_group(0).spawn() {
        Ok(_) => eprintln!("gex-elf: building the function tier for {path} at {base:#x} for the next run"),
        Err(e) => eprintln!("gex-elf: could not start building the function tier for {path}: {e}"),
    }
}

// ---------------------------------------------------------------------------
// Wine's own PE loader (stream W, W3; docs/wine-native.md §3.17)
// ---------------------------------------------------------------------------
//
// Under W3 Wine's ntdll.so maps every image itself and tells gex
// (`gex.image_mapped`, after its relocations); there is no NT layer and no
// section hook. Its i386 DLLs (ole32, rpcrt4, advapi32, combase, the
// programs...) were tier-1, and tier-1's units are not reusable across
// processes (each holds the page-table bases and the "already translated"
// set of the process that made it), so every Wine process recompiled
// megabytes of them: about 30 s of compile wall time per session boot.
// An image with a unit built for its bytes at its base (`gexc cgen-image
// PATH --pe-base BASE`) gets it here, as the NT layer's images do, and its
// compiled code comes from the module cache in every later process.
// Wine's own images only, by default (`GEX_W3_UNITS=all`: the program's
// too; `=0`: none); `GEX_W3_UNITS_BUILD=1` builds a missing
// unit in the background for the next run (off by default: Wine's DLLs get
// theirs at staging, `$W/cgen-w3.sh`).

/// The unit for an i386 image Wine's PE loader mapped at `base`, installed
/// if one was built. Never fatal: an image whose mapped code differs from
/// its unit's (or with none) stays on tier-1.
pub(crate) fn w3_image(caller: &mut Caller<'_, HostState>, path: &str, base: u64, pe: gex_ld::pe::PeImage) -> Result<()> {
    // 0: none; all: every image; otherwise Wine's own (its dll directory).
    // A program's images are not the default: Undertale's exe with its unit
    // never reached its first frame (2026-10-09, open).
    static WHICH: OnceLock<u8> = OnceLock::new();
    let which = *WHICH.get_or_init(|| match std::env::var("GEX_W3_UNITS").as_deref() {
        Ok("0") => 0,
        Ok("all") => 2,
        _ => 1,
    });
    // A library a stateful provider replaces (`libprov`: GEX_WASM_DLLS, an
    // ejected project's eject.json "providers"): bound in its place, with
    // no unit (none of its code runs), wherever it lives.
    if !crate::libprov::for_dll(&pe.exports.dll_name).is_empty() {
        return w3_stateful(caller, path, base, &pe);
    }
    if which == 0 || mode() == Mode::Off {
        return Ok(());
    }
    w3_trampolines(caller, !pe.is64)?;
    if (which == 1 && !path.contains("/lib/wine/")) || is_wasm_skeleton(&pe) {
        return Ok(());
    }
    // `GEX_W3_UNITS_SKIP=a.dll,b.dll`: those stay on tier-1 (bisecting).
    static SKIP: OnceLock<Vec<String>> = OnceLock::new();
    let skip = SKIP.get_or_init(|| std::env::var("GEX_W3_UNITS_SKIP").unwrap_or_default().split(',').map(|n| n.trim().to_ascii_lowercase()).filter(|n| !n.is_empty()).collect());
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    if skip.contains(&name) {
        return Ok(());
    }
    let p = Pending { path: path.to_string(), base, pe: Arc::new(pe) };
    w3_place(caller, p, true)
}

/// Bind the stateful providers of the image at `base` (`libprov`).
fn w3_stateful(caller: &mut Caller<'_, HostState>, path: &str, base: u64, pe: &gex_ld::pe::PeImage) -> Result<()> {
    let (shared, engine) = {
        let Some(jit) = caller.data().jit.as_ref() else { return Ok(()) };
        (Arc::clone(&jit.shared), jit.engine.clone())
    };
    let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let mut exports = HashMap::new();
    for e in pe.exports.entries.iter().filter(|e| e.rva != 0 && e.forwarder.is_none()) {
        for n in &e.names {
            exports.insert(n.clone(), e.rva);
        }
    }
    for lp in crate::libprov::for_dll(&pe.exports.dll_name) {
        let n = crate::libprov::install(lp, caller.data_mut(), &shared, &engine, &memory, &exports, base, pe.entry_rva)?;
        if trace() || std::env::var_os("GEX_W3_TRACE").is_some() {
            eprintln!("gex-w3: provider {} for {path}: {n} exports", lp.name);
        }
    }
    crate::ftab::catch_up(&mut *caller)?;
    replay_registry(caller)?;
    Ok(())
}

/// Images [`w3_image`] left for their first miss, by the process (its
/// `JitShared`) that mapped them.
static W3_DEFERRED: Mutex<Vec<(usize, Pending)>> = Mutex::new(Vec::new());

/// A tier-1 miss at `pc`: if it is inside an image [`w3_image`] deferred,
/// place that image's unit now.
pub(crate) fn w3_miss(caller: &mut Caller<'_, HostState>, pc: u64) -> Result<()> {
    let p = {
        let mut d = W3_DEFERRED.lock().unwrap_or_else(|e| e.into_inner());
        if d.is_empty() {
            return Ok(());
        }
        let owner = caller.data().jit.as_ref().map(|j| Arc::as_ptr(&j.shared) as usize).unwrap_or(0);
        let Some(i) = d.iter().position(|(o, p)| *o == owner && pc >= p.base && pc < p.base + p.pe.image_size as u64) else {
            return Ok(());
        };
        d.swap_remove(i).1
    };
    w3_place(caller, p, false)
}

/// Place `p`'s unit. `at_map`: Wine has only just mapped it, and an image
/// it must move is relocated later (ntdll's PE side, `perform_relocations`),
/// so a mismatch then defers it to its first miss rather than refusing it.
fn w3_place(caller: &mut Caller<'_, HostState>, p: Pending, at_map: bool) -> Result<()> {
    let (path, base) = (p.path.clone(), p.base);
    let t0 = std::time::Instant::now();
    let memory = caller.data().memory.clone().ok_or_else(|| anyhow!("guest memory unavailable"))?;
    let placed = match place(&p, memory.bytes(), at_map) {
        Ok(pl) => pl,
        Err(_) if at_map => {
            let owner = caller.data().jit.as_ref().map(|j| Arc::as_ptr(&j.shared) as usize).unwrap_or(0);
            W3_DEFERRED.lock().unwrap_or_else(|e| e.into_inner()).push((owner, p));
            return Ok(());
        }
        Err(e) => {
            eprintln!("gex-w3: {e:#}; it runs on tier-1");
            return Ok(());
        }
    };
    let Some(pl) = placed else {
        if trace() || std::env::var_os("GEX_W3_TRACE").is_some() {
            let key = gex_xlate::func::pe_input(&p.pe, base)
                .map(|(image, _)| unit_key(&image, &provided(&p.pe, base)))
                .unwrap_or_else(|e| format!("(not placeable: {e})"));
            eprintln!("gex-w3: {path} at {base:#x}: no function-tier unit (key {key}), tier-1");
        }
        if std::env::var("GEX_W3_UNITS_BUILD").as_deref() == Ok("1") {
            match spawn_build(&p) {
                Ok(child) => drop(child),
                Err(e) => eprintln!("gex-w3: could not start building the function tier for {path}: {e}"),
            }
        }
        return Ok(());
    };
    let (shared, engine, cache) = {
        let jit = caller.data().jit.as_ref().ok_or_else(|| anyhow!("runtime translation is not set up"))?;
        (Arc::clone(&jit.shared), jit.engine.clone(), jit.cache.clone())
    };
    // A unit not yet compiled for this host is compiled in the background,
    // and this process runs the image on tier-1: compiling here would stop
    // the guest for as long as Cranelift takes (d3dx9_43's 7.9 MB unit:
    // ten minutes; Undertale never reached a frame).
    if cache.is_some() && !pl.path.with_extension("cwasm").exists() {
        compile_later(pl.path.clone(), &pl.cwasm);
        if trace() || std::env::var_os("GEX_W3_TRACE").is_some() {
            eprintln!("gex-w3: {path} at {base:#x}: its unit is compiling in the background; tier-1 this time");
        }
        return Ok(());
    }
    {
        let _one = PLACING.lock().unwrap_or_else(|e| e.into_inner());
        install_placed(&p, pl, &shared, &engine, cache.as_ref(), &memory, t0)?;
    }
    // Its export providers, bound after its unit as an NT image's are (and
    // as the page binds a W3 image's, cgen.rs `place`): Wine's msvcp120
    // `_Thrd_current`, docs/wine-overhead.md F1.
    install_providers(&p, caller.data_mut(), &shared, &engine, &memory)?;
    crate::ftab::catch_up(&mut *caller)?;
    replay_registry(caller)?;
    Ok(())
}

// The test decodes i386 code: only an x86 guest build has that decoder.
#[cfg(all(test, feature = "translate", feature = "x86"))]
mod region_tests {
    #[test]
    fn a_32_bit_region_reads_an_absolute_slot() {
        let (mut image, _) = gex_kernel::win::trampoline_input(true);
        let at = image.segments[0].vaddr;
        let slot = (at + 0x100) as u32;
        let bytes = &mut image.segments[0].bytes;
        bytes[..6].copy_from_slice(&[0xff, 0x15, 0, 0, 0, 0]);
        bytes[2..6].copy_from_slice(&slot.to_le_bytes());
        bytes[6] = 0xc3;
        gex_xlate::set_compat(false);
        let code = super::region_code(&image);
        let mut starts = std::collections::BTreeMap::new();
        let f = gex_xlate::func::recover_one(&code, at, &mut starts, &Default::default(), &Default::default());
        let ir = format!("{:?}", f.blocks);
        let wrong = slot as u64 + at + 6;
        assert!(ir.contains(&format!("({slot})")), "no load of {slot:#x}: {ir}");
        assert!(!ir.contains(&format!("({wrong})")), "the slot was read relative to the next instruction");
    }
}
